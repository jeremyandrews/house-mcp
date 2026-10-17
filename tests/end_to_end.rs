//! The whole server over HTTP: the MCP tools through the rmcp client, and the
//! plain routes beside them.

mod common;

use base64::{Engine, engine::general_purpose::STANDARD};
use common::{
    drawing,
    mcp::{RO, RW, call, client, column, ok, sorted_names, start, svg_of},
    on_layer, rectangle, save, two_room_dxf,
};
use resvg::tiny_skia::Pixmap;
use rmcp::{RoleClient, model::CallToolResult, service::RunningService};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[tokio::test]
async fn end_to_end() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;

    // The read only token sees only reading tools.
    let rw_tools: Vec<String> = rw
        .list_all_tools()
        .await
        .expect("tools")
        .into_iter()
        .map(|t| t.name.into())
        .collect();
    let ro_tools: Vec<String> = ro
        .list_all_tools()
        .await
        .expect("tools")
        .into_iter()
        .map(|t| t.name.into())
        .collect();
    assert_eq!(rw_tools.len(), 32, "{rw_tools:?}");
    assert_eq!(
        ro_tools,
        [
            "connections",
            "context",
            "find",
            "get_detail",
            "get_file",
            "get_floor",
            "history",
            "in_space",
            "list_files",
            "list_sites",
            "measure",
            "nearest",
            "render_plan",
            "search_notes",
            "what_is_at"
        ]
    );

    // A site whose origin is 9 E, 45 N, and its ground floor.
    let site = ok(
        &rw,
        "create_site",
        json!({"name": "Test house", "origin": {"epsg": 4326, "x": 9.0, "y": 45.0}, "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("site id");
    let floor = ok(
        &rw,
        "upsert_floor",
        json!({"site_id": site, "name": "Ground", "level": 0, "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("floor id");

    // import_dxf reads only a stored file, so the drawing is uploaded first.
    let dxf = two_room_dxf();
    let file = ok(
        &rw,
        "upload_file",
        json!({"name": "two rooms.dxf", "base64": STANDARD.encode(&dxf), "links": {"site_id": site}, "reason": "surveyor's drawing"}),
    )
    .await;
    let sha = file["sha256"].as_str().expect("sha256").to_owned();
    assert_eq!(sha, hex::encode(Sha256::digest(&dxf)));
    let layers = json!({"A-ROOM": {"space": "room"}, "E-LIGHT": {"object": "light"}});
    let import = ok(
        &rw,
        "import_dxf",
        json!({"file_sha256": sha, "floor_id": floor, "layers": layers, "reason": "first import"}),
    )
    .await;
    assert_eq!(sorted_names(&import, "spaces"), ["Kitchen", "Living room"]);
    assert_eq!(sorted_names(&import, "objects"), ["LIGHT"]);
    assert_eq!(import["unit"], "millimetres");
    assert_eq!(import["linework"], 5);
    let light = import["objects"][0]["id"].as_i64().expect("light id");

    // what_is_at finds the room around a point and the light within a radius.
    let at = ok(
        &rw,
        "what_is_at",
        json!({"site_id": site, "floor_id": floor, "x": 1.0, "y": 1.0}),
    )
    .await;
    assert_eq!(sorted_names(&at, "spaces"), ["Kitchen"]);
    let at = ok(
        &rw,
        "what_is_at",
        json!({"site_id": site, "floor_id": floor, "x": 7.0, "y": 1.0, "radius": 0.5}),
    )
    .await;
    assert_eq!(sorted_names(&at, "spaces"), ["Living room"]);
    assert_eq!(at["objects"], json!([]));
    let at = ok(
        &ro,
        "what_is_at",
        json!({"site_id": site, "floor_id": floor, "x": 2.0, "y": 2.0, "radius": 1.0}),
    )
    .await;
    assert_eq!(at["objects"][0]["id"], light);
    assert!((at["objects"][0]["distance"].as_f64().expect("distance") - 0.5).abs() < 1e-9);

    let nearest = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "light", "from": {"point": [8.0, 1.0]}}),
    )
    .await;
    assert_eq!(nearest["objects"][0]["id"], light);
    let distance = nearest["objects"][0]["distance"].as_f64().expect("distance");
    assert!((distance - 6.0_f64.hypot(1.5)).abs() < 1e-9, "{distance}");

    let kitchen = import["spaces"]
        .as_array()
        .expect("spaces")
        .iter()
        .find(|s| s["name"] == "Kitchen")
        .expect("kitchen")["id"]
        .as_i64()
        .expect("kitchen id");
    let measured = ok(&ro, "measure", json!({"a": {"space": kitchen}, "b": {"object": light}})).await;
    assert!((measured["a"]["area"].as_f64().expect("area") - 12.0).abs() < 1e-9);
    assert!((measured["a"]["perimeter"].as_f64().expect("perimeter") - 14.0).abs() < 1e-9);
    assert_eq!(measured["distance"], 0.0);
    let inside = ok(&ro, "in_space", json!({"space_id": kitchen})).await;
    assert_eq!(inside["objects"][0]["id"], light);

    // render_plan returns a PNG at the default size and a short summary, and
    // the SVG with both room names only when asked.
    let plan = call(
        &ro,
        "render_plan",
        json!({"site_id": site, "floor_id": floor, "highlight": [{"object": light}]}),
    )
    .await
    .expect("render_plan");
    let image = plan.content.iter().find_map(|c| c.as_image()).expect("image content");
    assert_eq!(image.mime_type, "image/png");
    let png = STANDARD.decode(&image.data).expect("base64");
    let pixmap = Pixmap::decode_png(&png).expect("a PNG");
    assert_eq!(pixmap.width(), 1600);
    assert!(svg_of(&plan).is_none(), "no SVG unless asked");
    let summary = plan.content.iter().find_map(|c| c.as_text()).expect("summary");
    let summary: Value = serde_json::from_str(&summary.text).expect("JSON");
    assert_eq!(summary["width"], 1600);
    assert_eq!(summary["legend"], json!([]));
    let plan = call(
        &ro,
        "render_plan",
        json!({"site_id": site, "floor_id": floor, "include_svg": true}),
    )
    .await
    .expect("render_plan");
    assert_eq!(plan.content.iter().filter(|c| c.as_image().is_some()).count(), 1);
    let svg = svg_of(&plan).expect("SVG when asked");
    assert!(svg.contains(">Kitchen</text>") && svg.contains(">Living room</text>"));

    // A parcel from the land registry in longitude and latitude, about 30 by
    // 45 metres around the site origin.
    let parcel = json!({"type": "FeatureCollection", "features": [{
        "type": "Feature",
        "properties": {"parcel": "Foglio 1, mappale 23"},
        "geometry": {"type": "Polygon", "coordinates": [[[8.9998, 44.9998], [9.0002, 44.9998], [9.0002, 45.0002], [8.9998, 45.0002], [8.9998, 44.9998]]]}
    }]});
    let imported = ok(
        &rw,
        "import_geojson",
        json!({"site_id": site, "geojson": parcel, "epsg": 4326, "as": "space", "kind": "parcel", "name_property": "parcel", "reason": "land registry"}),
    )
    .await;
    assert_eq!(imported["created"][0]["name"], "Foglio 1, mappale 23");
    let parcel_id = imported["created"][0]["id"].as_i64().expect("parcel id");
    let outdoors = ok(&ro, "what_is_at", json!({"site_id": site, "x": 0.0, "y": 0.0})).await;
    assert_eq!(outdoors["spaces"][0]["id"], parcel_id);
    let area = ok(&ro, "measure", json!({"a": {"space": parcel_id}})).await["a"]["area"]
        .as_f64()
        .expect("area");
    assert!((1300.0..1500.0).contains(&area), "{area}");

    // The read only token is refused every write, and the refused call
    // leaves nothing behind.
    let refused = call(&ro, "create_site", json!({"name": "Not allowed", "reason": "test"}))
        .await
        .expect_err("refused");
    assert!(refused.contains("read and write token"), "{refused}");
    let sites = ok(&ro, "list_sites", json!({})).await;
    assert_eq!(sites.as_array().expect("sites").len(), 1);

    // Importing the same drawing again replaces the first import.
    let again = ok(
        &rw,
        "import_dxf",
        json!({"file_sha256": sha, "floor_id": floor, "layers": layers, "reason": "second import"}),
    )
    .await;
    assert_eq!(again["replaced_import_id"], import["import_id"]);
    let floor_now = ok(&ro, "get_floor", json!({"floor_id": floor})).await;
    assert_eq!(sorted_names(&floor_now, "spaces"), ["Kitchen", "Living room"]);
    assert_eq!(floor_now["objects"].as_array().expect("objects").len(), 1);

    // History shows every change to the kitchen: made, then retired by the
    // second import.
    let history = ok(&ro, "history", json!({"kind": "space", "id": kitchen})).await;
    let entries: Vec<(&str, &str, &str)> = history
        .as_array()
        .expect("history")
        .iter()
        .map(|h| {
            (
                h["operation"].as_str().expect("op"),
                h["author"].as_str().expect("author"),
                h["reason"].as_str().expect("reason"),
            )
        })
        .collect();
    assert_eq!(
        entries,
        [
            ("UPDATE", "jesse", "second import"),
            ("INSERT", "jesse", "first import")
        ]
    );
    for (kind, id) in [
        ("site", site),
        ("floor", floor),
        ("object", light),
        ("space", parcel_id),
        ("file", file["id"].as_i64().expect("file id")),
    ] {
        let history = ok(&ro, "history", json!({"kind": kind, "id": id})).await;
        let first = history
            .as_array()
            .and_then(|h| h.last())
            .unwrap_or_else(|| panic!("{kind} {id} has no history"));
        assert_eq!(
            (first["operation"].as_str(), first["author"].as_str()),
            (Some("INSERT"), Some("jesse")),
            "{kind}"
        );
    }

    // A note removed outright from QGIS comes back from its history.
    let note = ok(
        &rw,
        "add_note",
        json!({"body": "Stopcock under the sink", "links": {"space_id": kitchen}, "reason": "walked the house"}),
    )
    .await["id"]
        .as_i64()
        .expect("note id");
    let mut qgis = server.db.qgis().await;
    sqlx::query("DELETE FROM note WHERE id = $1")
        .bind(note)
        .execute(&mut qgis)
        .await
        .expect("delete");
    let found = ok(&ro, "search_notes", json!({"query": "stopcock"})).await;
    assert_eq!(found, json!([]));
    ok(
        &rw,
        "restore",
        json!({"kind": "note", "id": note, "reason": "deleted by mistake"}),
    )
    .await;
    let found = ok(&ro, "search_notes", json!({"query": "stopcock"})).await;
    assert_eq!(found[0]["id"], note);

    let light = again["objects"][0]["id"].as_i64().expect("new light id");
    // A soft delete hides a thing until it is restored.
    ok(&rw, "delete", json!({"kind": "object", "id": light, "reason": "test"})).await;
    let nearest = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "light", "from": {"point": [8.0, 1.0]}}),
    )
    .await;
    assert_eq!(nearest["objects"], json!([]));
    ok(&rw, "restore", json!({"kind": "object", "id": light, "reason": "test"})).await;
    let nearest = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "light", "from": {"point": [8.0, 1.0]}}),
    )
    .await;
    assert_eq!(nearest["objects"][0]["id"], light);

    // A DWG is kept but not read.
    let dwg = ok(
        &rw,
        "upload_file",
        json!({"name": "plan.dwg", "base64": STANDARD.encode(b"AC1032 not really a drawing"), "reason": "test"}),
    )
    .await;
    let refused = call(
        &rw,
        "import_dxf",
        json!({"file_sha256": dwg["sha256"], "floor_id": floor, "reason": "test"}),
    )
    .await
    .expect_err("DWG refused");
    assert!(refused.contains("ODA File Converter"), "{refused}");

    // Bad input names its field.
    let refused = call(
        &rw,
        "upsert_space",
        json!({"site_id": site, "name": "x", "kind": "attic", "geometry": {}, "reason": "test"}),
    )
    .await
    .expect_err("refused");
    assert!(refused.contains("kind: must be one of"), "{refused}");

    rw.cancel().await.expect("close");
    ro.cancel().await.expect("close");
}

#[tokio::test]
async fn plain_http_routes() {
    let server = start().await;
    let http = reqwest::Client::new();

    let health = http
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .expect("healthz");
    assert_eq!(health.status(), 200);
    for path in [
        "/",
        "/api/files/0000000000000000000000000000000000000000000000000000000000000000",
        "/mcp",
    ] {
        let response = http
            .get(format!("{}{path}", server.base))
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), 401, "{path} without a token");
    }

    // Upload over HTTP and read back the same bytes.
    let body = b"<b>not markup</b> in a text file".to_vec();
    let upload = |token: &str| {
        http.post(format!("{}/api/files?name=%3Cimg%3E.txt&reason=testing", server.base))
            .bearer_auth(token)
            .header("content-type", "text/plain")
            .body(body.clone())
            .send()
    };
    assert_eq!(upload(RO).await.expect("upload").status(), 403);
    let stored: Value = upload(RW).await.expect("upload").json().await.expect("json");
    let sha = stored["sha256"].as_str().expect("sha256");
    assert_eq!(sha, hex::encode(Sha256::digest(&body)));
    let download = http
        .get(format!("{}/api/files/{sha}", server.base))
        .bearer_auth(RO)
        .send()
        .await
        .expect("download");
    assert_eq!(download.status(), 200);
    assert!(
        download.headers()["content-disposition"]
            .to_str()
            .expect("header")
            .starts_with("attachment")
    );
    assert_eq!(download.headers()["x-content-type-options"], "nosniff");
    assert_eq!(download.bytes().await.expect("bytes").as_ref(), body.as_slice());
    let bad = http
        .get(format!("{}/api/files/..%2F..%2Fetc%2Fpasswd", server.base))
        .bearer_auth(RO)
        .send()
        .await
        .expect("request");
    assert_eq!(bad.status(), 400);

    // The page needs the read and write token as its password and escapes
    // what it lists.
    let page = http
        .get(format!("{}/", server.base))
        .basic_auth("jeremy", Some(RO))
        .send()
        .await
        .expect("page");
    assert_eq!(page.status(), 401);
    let page = http
        .get(format!("{}/", server.base))
        .basic_auth("jeremy", Some(RW))
        .send()
        .await
        .expect("page");
    assert_eq!(page.status(), 200);
    let policy = page.headers()["content-security-policy"]
        .to_str()
        .expect("csp")
        .to_owned();
    assert!(policy.starts_with("default-src 'none'"), "{policy}");
    let html = page.text().await.expect("html");
    assert!(html.contains("&lt;img&gt;.txt") && !html.contains("<img>"));

    // A form post must come from the page's own origin.
    let host = server.base.trim_start_matches("http://").to_owned();
    let form = || {
        reqwest::multipart::Form::new().text("reason", "test").part(
            "file",
            reqwest::multipart::Part::bytes(b"hello".to_vec()).file_name("hello.txt"),
        )
    };
    let cross = http
        .post(format!("{}/", server.base))
        .basic_auth("jeremy", Some(RW))
        .header("origin", "https://elsewhere.example")
        .multipart(form())
        .send()
        .await
        .expect("post");
    assert_eq!(cross.status(), 403);
    let same = http
        .post(format!("{}/", server.base))
        .basic_auth("jeremy", Some(RW))
        .header("origin", format!("http://{host}"))
        .multipart(form())
        .send()
        .await
        .expect("post");
    assert!(
        same.status().is_success() || same.status().is_redirection(),
        "{}",
        same.status()
    );
}

#[tokio::test]
async fn awkward_input_is_refused_or_survived() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let site = ok(&rw, "create_site", json!({"name": "Edge", "reason": "test"})).await["id"]
        .as_i64()
        .expect("site id");
    let floor = ok(
        &rw,
        "upsert_floor",
        json!({"site_id": site, "name": "Ground", "level": 0, "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("floor id");

    // Empty geometry from a caller is refused by name.
    let refused = call(
        &rw,
        "upsert_space",
        json!({"site_id": site, "floor_id": floor, "name": "Nothing", "kind": "room",
               "geometry": {"type": "Polygon", "coordinates": []}, "reason": "test"}),
    )
    .await
    .expect_err("refused");
    assert!(refused.contains("geometry: is empty"), "{refused}");

    // QGIS cannot store one either, nor a curve, which no tool could read
    // back as GeoJSON.
    let mut qgis = server.db.qgis().await;
    for geometry in ["MULTIPOINT EMPTY", "POINT EMPTY", "CIRCULARSTRING(0 0, 1 1, 2 0)"] {
        let error = sqlx::query(
            "INSERT INTO object (site_id, floor_id, kind, name, geom) VALUES ($1, $2, 'light', 'Ghost', $3::geometry)",
        )
        .bind(site)
        .bind(floor)
        .bind(geometry)
        .execute(&mut qgis)
        .await
        .expect_err(geometry);
        let code = error.as_database_error().and_then(|e| e.code()).map(|c| c.into_owned());
        assert_eq!(code.as_deref(), Some("23514"), "{geometry}: {error}");
    }
    let error = sqlx::query(
        "INSERT INTO space (site_id, floor_id, kind, name, geom) VALUES ($1, $2, 'room', 'Ghost', 'POLYGON EMPTY')",
    )
    .bind(site)
    .bind(floor)
    .execute(&mut qgis)
    .await
    .expect_err("empty space");
    assert!(error.to_string().contains("space_geom_check"), "{error}");

    // Soft deleted, then removed outright from QGIS: restore brings it back live.
    let note = ok(
        &rw,
        "add_note",
        json!({"body": "Meter in the garage", "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("note id");
    ok(&rw, "delete", json!({"kind": "note", "id": note, "reason": "test"})).await;
    sqlx::query("DELETE FROM note WHERE id = $1")
        .bind(note)
        .execute(&mut qgis)
        .await
        .expect("delete");
    ok(&rw, "restore", json!({"kind": "note", "id": note, "reason": "test"})).await;
    let found = ok(&rw, "search_notes", json!({"query": "garage"})).await;
    assert_eq!(found[0]["id"], note);

    // A text inside a room, longer than a name may be, still names it, cut short.
    let mut drawing = common::drawing(dxf::enums::Units::Meters);
    drawing.add_entity(common::on_layer("A-ROOM", common::rectangle(0.0, 0.0, 4.0, 3.0)));
    drawing.add_entity(common::on_layer(
        "A-TEXT",
        dxf::entities::EntityType::Text(dxf::entities::Text {
            location: dxf::Point::new(1.0, 1.0, 0.0),
            text_height: 0.2,
            value: "x".repeat(300),
            ..Default::default()
        }),
    ));
    let file = ok(
        &rw,
        "upload_file",
        json!({"name": "long.dxf", "base64": STANDARD.encode(common::save(&drawing)), "reason": "test"}),
    )
    .await;
    let import = ok(
        &rw,
        "import_dxf",
        json!({"file_sha256": file["sha256"], "floor_id": floor, "layers": {"A-ROOM": {"space": "room"}}, "reason": "test"}),
    )
    .await;
    assert_eq!(import["spaces"][0]["name"], "x".repeat(200));
}

#[tokio::test]
async fn limits_and_refusals_name_their_field() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let http = reqwest::Client::new();
    let site = ok(
        &rw,
        "create_site",
        json!({"name": "Limits", "origin": {"epsg": 4326, "x": 9.0, "y": 45.0}, "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("site id");
    let floor = ok(
        &rw,
        "upsert_floor",
        json!({"site_id": site, "name": "Ground", "level": 0, "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("floor id");

    // Uploads over the tool limit name the field that carried them.
    let tool_limit = 25 * 1024 * 1024;
    let refused = call(
        &rw,
        "upload_file",
        json!({"name": "big.txt", "text": "x".repeat(tool_limit + 1), "reason": "test"}),
    )
    .await
    .expect_err("refused");
    assert!(refused.contains("text: is larger than 25 MiB"), "{refused}");
    let refused = call(
        &rw,
        "upload_file",
        json!({"name": "big.bin", "base64": STANDARD.encode(vec![0u8; tool_limit + 1]), "reason": "test"}),
    )
    .await
    .expect_err("refused");
    assert!(refused.contains("base64: decodes to more than 25 MiB"), "{refused}");

    // A file larger than an import reads is refused before it is parsed.
    let big = http
        .post(format!("{}/api/files?name=huge.dxf&reason=test", server.base))
        .bearer_auth(RW)
        .body(vec![b' '; 16 * 1024 * 1024 + 1])
        .send()
        .await
        .expect("upload");
    let big: Value = big.json().await.expect("json");
    let refused = call(
        &rw,
        "import_dxf",
        json!({"file_sha256": big["sha256"], "floor_id": floor, "reason": "test"}),
    )
    .await
    .expect_err("refused");
    assert!(refused.contains("file_sha256: is larger than 16 MiB"), "{refused}");
    let refused = call(
        &rw,
        "import_geojson",
        json!({"site_id": site, "file_sha256": big["sha256"], "as": "space", "kind": "parcel", "reason": "test"}),
    )
    .await
    .expect_err("refused");
    assert!(refused.contains("file_sha256: is larger than 16 MiB"), "{refused}");

    // A deleted file is gone from every route that reads files.
    let file = ok(
        &rw,
        "upload_file",
        json!({"name": "gone.dxf", "base64": STANDARD.encode(two_room_dxf()), "reason": "test"}),
    )
    .await;
    let sha = file["sha256"].as_str().expect("sha256").to_owned();
    ok(
        &rw,
        "delete",
        json!({"kind": "file", "id": file["id"], "reason": "test"}),
    )
    .await;
    let download = http
        .get(format!("{}/api/files/{sha}", server.base))
        .bearer_auth(RW)
        .send()
        .await
        .expect("download");
    assert_eq!(download.status(), 404);
    let refused = call(
        &rw,
        "import_dxf",
        json!({"file_sha256": sha, "floor_id": floor, "reason": "test"}),
    )
    .await
    .expect_err("refused");
    assert!(refused.contains("no live file with this hash"), "{refused}");

    // Postgres refuses a NUL in text; the caller hears why.
    let refused = call(&rw, "add_note", json!({"body": "fine", "reason": "a\0b"}))
        .await
        .expect_err("refused");
    assert!(refused.contains("reason: contains a NUL character"), "{refused}");
    let refused = call(&rw, "add_note", json!({"body": "a\0b", "reason": "test"}))
        .await
        .expect_err("refused");
    assert!(refused.contains("input: contains a NUL character"), "{refused}");

    // Empty geometry in a GeoJSON import is refused by feature.
    for target in ["space", "object"] {
        let empty = json!({"type": "Feature", "properties": {}, "geometry": {"type": "Polygon", "coordinates": []}});
        let refused = call(
            &rw,
            "import_geojson",
            json!({"site_id": site, "geojson": empty, "as": target, "kind": "parcel", "reason": "test"}),
        )
        .await
        .expect_err("refused");
        assert!(refused.contains("features[0]: is empty"), "{target}: {refused}");
    }

    // A room's name is its tallest text that is not blank, trimmed.
    let mut drawing = common::drawing(dxf::enums::Units::Meters);
    drawing.add_entity(common::on_layer("A-ROOM", common::rectangle(0.0, 0.0, 4.0, 3.0)));
    for (value, height, x) in [("  Pantry  ", 0.2, 1.0), ("   ", 0.5, 2.0)] {
        drawing.add_entity(common::on_layer(
            "A-TEXT",
            dxf::entities::EntityType::Text(dxf::entities::Text {
                location: dxf::Point::new(x, 1.0, 0.0),
                text_height: height,
                value: value.into(),
                ..Default::default()
            }),
        ));
    }
    let file = ok(
        &rw,
        "upload_file",
        json!({"name": "pantry.dxf", "base64": STANDARD.encode(common::save(&drawing)), "reason": "test"}),
    )
    .await;
    let import = ok(
        &rw,
        "import_dxf",
        json!({"file_sha256": file["sha256"], "floor_id": floor, "layers": {"A-ROOM": {"space": "room"}}, "reason": "test"}),
    )
    .await;
    assert_eq!(import["spaces"][0]["name"], "Pantry");
}

#[tokio::test]
async fn geometry_names_and_reasons_are_checked_and_named() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let http = reqwest::Client::new();
    let site = ok(
        &rw,
        "create_site",
        json!({"name": "Checks", "origin": {"epsg": 4326, "x": 9.0, "y": 45.0}, "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("site id");
    let refused = |result: Result<CallToolResult, String>| result.expect_err("refused");

    // Every geometry type an object may have is accepted.
    for geometry in [
        json!({"type": "Point", "coordinates": [1.0, 1.0]}),
        json!({"type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0]]}),
        json!({"type": "Polygon", "coordinates": [[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]]}),
        json!({"type": "MultiPoint", "coordinates": [[0.0, 0.0], [1.0, 1.0]]}),
        json!({"type": "MultiLineString", "coordinates": [[[0.0, 0.0], [1.0, 1.0]], [[2.0, 2.0], [3.0, 3.0]]]}),
        json!({"type": "MultiPolygon", "coordinates": [[[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]]]}),
    ] {
        ok(
            &rw,
            "upsert_object",
            json!({"site_id": site, "kind": "pipe", "name": "p", "geometry": geometry, "reason": "test"}),
        )
        .await;
    }
    let collection =
        json!({"type": "GeometryCollection", "geometries": [{"type": "Point", "coordinates": [1.0, 1.0]}]});
    let error = refused(
        call(
            &rw,
            "upsert_object",
            json!({"site_id": site, "kind": "pipe", "name": "p", "geometry": collection, "reason": "test"}),
        )
        .await,
    );
    assert!(error.contains("geometry: is a GEOMETRYCOLLECTION"), "{error}");

    // A space must be one valid polygon of a bounded size.
    let line = json!({"type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0]]});
    let error = refused(
        call(
            &rw,
            "upsert_space",
            json!({"site_id": site, "name": "s", "kind": "room", "geometry": line, "reason": "test"}),
        )
        .await,
    );
    assert!(error.contains("geometry: is a LINESTRING"), "{error}");
    let bowtie =
        json!({"type": "Polygon", "coordinates": [[[0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0], [0.0, 0.0]]]});
    let error = refused(
        call(
            &rw,
            "upsert_space",
            json!({"site_id": site, "name": "s", "kind": "room", "geometry": bowtie, "reason": "test"}),
        )
        .await,
    );
    assert!(error.contains("geometry: is not valid"), "{error}");
    let long: Vec<[f64; 2]> = (0..10_001).map(|i| [f64::from(i), 0.0]).collect();
    let error = refused(
        call(
            &rw,
            "upsert_object",
            json!({"site_id": site, "kind": "cable_run", "name": "c", "geometry": {"type": "LineString", "coordinates": long}, "reason": "test"}),
        )
        .await,
    );
    assert!(error.contains("geometry: has more than 10000 vertices"), "{error}");

    // A NUL inside jsonb is refused by name, like one in text.
    let error = refused(
        call(
            &rw,
            "upsert_object",
            json!({"site_id": site, "kind": "light", "name": "l", "geometry": {"type": "Point", "coordinates": [0.0, 0.0]}, "refs": {"ha_entity_id": "a\u{0}b"}, "reason": "test"}),
        )
        .await,
    );
    assert!(error.contains("input: contains a NUL character"), "{error}");

    // Imported features are checked the same way, and name the feature.
    let feature =
        |geometry: Value, name: &str| json!({"type": "Feature", "properties": {"name": name}, "geometry": geometry});
    let near_origin = json!({"type": "LineString", "coordinates": [[9.0, 45.0], [9.0001, 45.0001]]});
    for (geometry, name, expected) in [
        (near_origin.clone(), "line", "features[0]: is a LINESTRING"),
        (
            json!({"type": "Point", "coordinates": [10.0, 1000.0]}),
            "far",
            "features[0]: coordinates cannot be converted from EPSG:4326",
        ),
        (
            json!({"type": "Point", "coordinates": [9.0, 45.0]}),
            "   ",
            "features[0]: has a blank name",
        ),
        (
            json!({"type": "Point", "coordinates": [9.0, 45.0]}),
            &"n".repeat(201),
            "features[0]: has a name longer than 200 characters",
        ),
    ] {
        let target = if expected.contains("LINESTRING") {
            "space"
        } else {
            "object"
        };
        let error = refused(
            call(
                &rw,
                "import_geojson",
                json!({"site_id": site, "geojson": feature(geometry, name), "as": target, "kind": if target == "space" { "parcel" } else { "tree" }, "name_property": "name", "reason": "test"}),
            )
            .await,
        );
        assert!(error.contains(expected), "{expected}: {error}");
    }

    // An origin that cannot be placed in UTM zone 32N is refused by name.
    for origin in [
        json!({"epsg": 32632, "x": 1e15, "y": 1e15}),
        json!({"epsg": 4326, "x": 10.0, "y": 1000.0}),
        // South of the equator, which zone 32N does not cover.
        json!({"epsg": 4326, "x": 9.0, "y": -1.0}),
        // East of the zone's easting range.
        json!({"epsg": 32632, "x": 900000.0, "y": 5000000.0}),
    ] {
        let error = refused(
            call(
                &rw,
                "create_site",
                json!({"name": "Nowhere", "origin": origin, "reason": "test"}),
            )
            .await,
        );
        assert!(error.contains("origin: cannot be placed in UTM zone 32N"), "{error}");
    }

    // A blank reason is refused on every upload path.
    let error = refused(
        call(
            &rw,
            "upload_file",
            json!({"name": "a.txt", "text": "a", "reason": "   "}),
        )
        .await,
    );
    assert!(error.contains("reason: is required"), "{error}");
    let blank = http
        .post(format!("{}/api/files?name=b.txt&reason=%20", server.base))
        .bearer_auth(RW)
        .body("b")
        .send()
        .await
        .expect("upload");
    assert_eq!(blank.status(), 400);

    // A file of exactly the import cap is read, not refused for its size.
    let at_cap = http
        .post(format!("{}/api/files?name=cap.dxf&reason=test", server.base))
        .bearer_auth(RW)
        .body(vec![b' '; 16 * 1024 * 1024])
        .send()
        .await
        .expect("upload");
    let at_cap: Value = at_cap.json().await.expect("json");
    let floor = ok(
        &rw,
        "upsert_floor",
        json!({"site_id": site, "name": "Ground", "level": 0, "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("floor id");
    let error = refused(
        call(
            &rw,
            "import_dxf",
            json!({"file_sha256": at_cap["sha256"], "floor_id": floor, "reason": "test"}),
        )
        .await,
    );
    assert!(!error.contains("larger than"), "{error}");
}

/// A georeferenced site with one floor, as the tests below start from.
async fn site_and_floor(rw: &RunningService<RoleClient, ()>, name: &str) -> (i64, i64) {
    let site = ok(
        rw,
        "create_site",
        json!({"name": name, "origin": {"epsg": 4326, "x": 9.0, "y": 45.0}, "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("site id");
    let floor = ok(
        rw,
        "upsert_floor",
        json!({"site_id": site, "name": "Ground", "level": 0, "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("floor id");
    (site, floor)
}

/// A ring 0.0001° on a side, about 8 m by 11 m, with its south west corner
/// at `lon`, 45° N.
fn square(lon: f64) -> Value {
    json!([[
        [lon, 45.0],
        [lon + 0.0001, 45.0],
        [lon + 0.0001, 45.0001],
        [lon, 45.0001],
        [lon, 45.0]
    ]])
}

/// A GeoJSON Feature with `name` as its one property.
fn named_feature(geometry: Value, name: &str) -> Value {
    json!({"type": "Feature", "properties": {"name": name}, "geometry": geometry})
}

#[tokio::test]
async fn nearest_and_what_is_at_list_the_nearest_first() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (site, floor) = site_and_floor(&rw, "Order").await;
    // Made in the order a, c, b at 5, 3 and 1 m, so nearest first is b, c, a,
    // which neither id order nor name order in either direction gives.
    let mut ids = Vec::new();
    for (name, x) in [("a", 5.0), ("c", 3.0), ("b", 1.0)] {
        let id = ok(
            &rw,
            "upsert_object",
            json!({"site_id": site, "floor_id": floor, "kind": "light", "name": name,
                   "geometry": {"type": "Point", "coordinates": [x, 0.0]}, "reason": "test"}),
        )
        .await["id"]
            .clone();
        ids.push(id);
    }
    let [a, c, b] = ids.try_into().expect("three lights");
    let nearest_first = [b, c, a];
    let found = ok(
        &rw,
        "nearest",
        json!({"site_id": site, "kind": "light", "from": {"point": [0.0, 0.0]}}),
    )
    .await;
    assert_eq!(column(&found["objects"], "id"), nearest_first);
    let at = ok(
        &rw,
        "what_is_at",
        json!({"site_id": site, "floor_id": floor, "x": 0.0, "y": 0.0, "radius": 10.0}),
    )
    .await;
    assert_eq!(column(&at["objects"], "id"), nearest_first);
}

#[tokio::test]
async fn list_files_keeps_to_what_a_file_is_linked_to() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (site, floor) = site_and_floor(&rw, "Links").await;
    let space = ok(
        &rw,
        "upsert_space",
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Hall", "reason": "test",
               "geometry": {"type": "Polygon", "coordinates": [[[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0], [0.0, 0.0]]]}}),
    )
    .await["id"]
        .clone();
    let object = ok(
        &rw,
        "upsert_object",
        json!({"site_id": site, "floor_id": floor, "kind": "light", "name": "Lamp", "reason": "test",
               "geometry": {"type": "Point", "coordinates": [1.0, 1.0]}}),
    )
    .await["id"]
        .clone();
    ok(
        &rw,
        "upload_file",
        json!({"name": "loose.txt", "text": "loose", "reason": "test"}),
    )
    .await;
    let links = [
        ("site.txt", json!({"site_id": site})),
        ("floor.txt", json!({"floor_id": floor})),
        ("space.txt", json!({"space_id": space})),
        ("object.txt", json!({"object_id": object})),
    ];
    for (name, link) in &links {
        ok(
            &rw,
            "upload_file",
            json!({"name": name, "text": name, "links": link, "reason": "test"}),
        )
        .await;
    }
    for (name, link) in &links {
        let files = ok(&rw, "list_files", link.clone()).await;
        assert_eq!(column(&files, "name"), [json!(name)], "list_files by {link}");
    }
}

#[tokio::test]
async fn geojson_names_are_trimmed_counted_in_characters_and_cut_with_their_part() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (site, _) = site_and_floor(&rw, "Names").await;
    let polygon = |lon| json!({"type": "Polygon", "coordinates": square(lon)});
    let two_parts = |lon| json!({"type": "MultiPolygon", "coordinates": [square(lon), square(lon + 0.0005)]});
    // 197 characters and a part suffix of four come to 201, so the cut takes
    // the closing bracket and nothing else.
    let long = "p".repeat(197);
    for (geometry, name, stored) in [
        (polygon(9.0), "  Garden  ".to_owned(), vec!["Garden".to_owned()]),
        (polygon(9.001), "é".repeat(200), vec!["é".repeat(200)]),
        (
            two_parts(9.002),
            "Bed".to_owned(),
            vec!["Bed (1)".to_owned(), "Bed (2)".to_owned()],
        ),
        (
            two_parts(9.003),
            long.clone(),
            vec![format!("{long} (1"), format!("{long} (2")],
        ),
    ] {
        let created = ok(
            &rw,
            "import_geojson",
            json!({"site_id": site, "geojson": named_feature(geometry, &name), "as": "space", "kind": "garden_bed",
                   "name_property": "name", "reason": "test"}),
        )
        .await;
        assert_eq!(column(&created["created"], "name"), stored);
    }
}

#[tokio::test]
async fn names_a_drawing_composes_are_numbered_and_cut_and_its_unit_is_stored() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (_, floor) = site_and_floor(&rw, "Drawing").await;
    // Short names show the numbering; long ones show the cut, which takes the
    // number with it.
    let (long_twice, long_once) = ("B".repeat(250), "C".repeat(250));
    let long_layer = "L".repeat(250);
    let mut dxf = drawing(dxf::enums::Units::Feet);
    for name in [long_twice.as_str(), long_once.as_str(), "Lamp", "Single"] {
        dxf.add_block(dxf::Block {
            name: name.into(),
            ..Default::default()
        });
    }
    let inserts = [
        (long_twice.as_str(), 1.0),
        (long_twice.as_str(), 2.0),
        (long_once.as_str(), 3.0),
        ("Lamp", 8.5),
        ("Lamp", 10.5),
        ("Single", 12.0),
    ];
    for (block, x) in inserts {
        dxf.add_entity(on_layer(
            "E-LIGHT",
            dxf::entities::EntityType::Insert(dxf::entities::Insert {
                name: block.into(),
                location: dxf::Point::new(x, 1.0, 0.0),
                ..Default::default()
            }),
        ));
    }
    // On the long layer one room with no text inside and one with a long
    // label; on the short layer two rooms with no text.
    dxf.add_entity(on_layer(&long_layer, rectangle(0.0, 0.0, 3.0, 3.0)));
    dxf.add_entity(on_layer(&long_layer, rectangle(4.0, 0.0, 7.0, 3.0)));
    dxf.add_entity(on_layer("R", rectangle(8.0, 0.0, 9.0, 3.0)));
    dxf.add_entity(on_layer("R", rectangle(10.0, 0.0, 11.0, 3.0)));
    dxf.add_entity(on_layer(
        "A-TEXT",
        dxf::entities::EntityType::Text(dxf::entities::Text {
            location: dxf::Point::new(5.0, 1.0, 0.0),
            text_height: 0.5,
            value: "T".repeat(250),
            ..Default::default()
        }),
    ));
    let sha = ok(
        &rw,
        "upload_file",
        json!({"name": "long.dxf", "base64": STANDARD.encode(save(&dxf)), "reason": "test"}),
    )
    .await["sha256"]
        .clone();
    let layers = json!({ "E-LIGHT": {"object": "light"}, long_layer: {"space": "room"}, "R": {"space": "room"} });
    let import = ok(
        &rw,
        "import_dxf",
        json!({"file_sha256": sha, "floor_id": floor, "layers": layers, "reason": "test"}),
    )
    .await;
    // Copies of a block used more than once are numbered and a single copy
    // is not; a space with no text inside is named by its layer and its place
    // among the drawing's outlines, here the third and fourth, and one with
    // text by the text; every name is cut at 200 characters.
    assert_eq!(
        sorted_names(&import, "objects"),
        [
            "B".repeat(200),
            "B".repeat(200),
            "C".repeat(200),
            "Lamp 1".into(),
            "Lamp 2".into(),
            "Single".into()
        ]
    );
    assert_eq!(
        sorted_names(&import, "spaces"),
        ["L".repeat(200), "R 3".into(), "R 4".into(), "T".repeat(200)]
    );
    let unit: String = sqlx::query_scalar("SELECT unit FROM dxf_import WHERE id = $1")
        .bind(import["import_id"].as_i64().expect("import id"))
        .fetch_one(&mut server.db.qgis().await)
        .await
        .expect("the import's row");
    assert_eq!(unit, "feet");
}

#[tokio::test]
async fn a_site_without_a_georeference_refuses_geographic_input() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let bare = ok(&rw, "create_site", json!({"name": "Bare", "reason": "test"})).await["id"]
        .as_i64()
        .expect("site id");
    let parcel = named_feature(json!({"type": "Polygon", "coordinates": square(9.0)}), "x");
    let error = call(
        &rw,
        "import_geojson",
        json!({"site_id": bare, "geojson": parcel, "as": "space", "kind": "parcel",
               "name_property": "name", "reason": "test"}),
    )
    .await
    .expect_err("refused");
    assert!(error.contains("has no georeference"), "{error}");
}

#[tokio::test]
async fn plan_labels_come_from_nicknames_then_english_names_then_names() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (site, floor) = site_and_floor(&rw, "Labels").await;
    let rooms = [
        ("Stanza uno", json!({"common_name": "the snug", "name_en": "Lounge"})),
        ("Stanza due", json!({"name_en": "pantry"})),
        ("stanza tre", json!({})),
        ("Stanza quattro", json!({"common_name": 7, "name_en": ""})),
        ("Stanza cinque", json!({"common_name": " ", "name_en": "Attic"})),
    ];
    for (i, (name, attributes)) in rooms.iter().enumerate() {
        let x = 4.0 * i as f64;
        let ring = json!([[[x, 0.0], [x + 3.0, 0.0], [x + 3.0, 3.0], [x, 3.0], [x, 0.0]]]);
        ok(
            &rw,
            "upsert_space",
            json!({"site_id": site, "floor_id": floor, "kind": "room", "name": name,
                   "geometry": {"type": "Polygon", "coordinates": ring}, "attributes": attributes, "reason": "test"}),
        )
        .await;
    }
    let labels = async |extra: Value| {
        let mut args = json!({"site_id": site, "floor_id": floor, "include_svg": true});
        args.as_object_mut()
            .expect("an object")
            .extend(extra.as_object().expect("an object").clone());
        let plan = call(&rw, "render_plan", args).await.expect("render_plan");
        let svg = svg_of(&plan).expect("svg");
        let mut found: Vec<String> = svg
            .split("</text>")
            .filter_map(|s| s.rsplit_once('>').map(|(_, text)| text.to_owned()))
            .filter(|text| !text.is_empty() && text != "N" && !text.ends_with(" m") && !text.starts_with("Labels"))
            .collect();
        found.sort();
        found
    };
    // The nickname without its article, capitalized; the English name
    // capitalized; the name as stored; and empty or non-string values
    // passed over.
    assert_eq!(
        labels(json!({})).await,
        ["Attic", "Pantry", "Snug", "Stanza quattro", "stanza tre"]
    );
    assert_eq!(
        labels(json!({"label_from": []})).await,
        [
            "Stanza cinque",
            "Stanza due",
            "Stanza quattro",
            "Stanza uno",
            "stanza tre"
        ]
    );
    assert_eq!(
        labels(json!({"label_from": ["name_en"]})).await,
        ["Attic", "Lounge", "Pantry", "Stanza quattro", "stanza tre"]
    );
}

/// A floor plan 6 by 3 metres in millimetres: a frame, a wall drawn as its
/// centre line along y = 1.5 m, and the wall's two faces 0.2 m either side.
fn walled_dxf() -> Vec<u8> {
    use dxf::{Point, entities::Line};
    let mut drawing = drawing(dxf::enums::Units::Millimeters);
    drawing.add_entity(on_layer("A-WALL", rectangle(0.0, 0.0, 6000.0, 3000.0)));
    for y in [1300.0, 1500.0, 1700.0] {
        drawing.add_entity(on_layer(
            "A-WALL",
            dxf::entities::EntityType::Line(Line::new(Point::new(0.0, y, 0.0), Point::new(6000.0, y, 0.0))),
        ));
    }
    save(&drawing)
}

/// The pixel of a site point on the walled floor at the default size: 40 px
/// margins and 1480 / 6 px per metre.
fn walled_px(x: f64, y: f64) -> (u32, u32) {
    let scale = 1480.0 / 6.0;
    (
        (40.0 + x * scale).round() as u32,
        (40.0 + (3.0 - y) * scale).round() as u32,
    )
}

/// Whether anything is drawn within 4 px of a site point.
fn inked(pixmap: &Pixmap, x: f64, y: f64) -> bool {
    let (cx, cy) = walled_px(x, y);
    (cx - 4..=cx + 4)
        .flat_map(|x| (cy - 4..=cy + 4).map(move |y| (x, y)))
        .any(|(x, y)| {
            let p = pixmap.pixel(x, y).expect("inside");
            (p.red(), p.green(), p.blue()) != (255, 255, 255)
        })
}

struct Drawn {
    pixmap: Pixmap,
    svg: String,
}

async fn draw(client: &RunningService<RoleClient, ()>, mut args: Value) -> Drawn {
    args["include_svg"] = json!(true);
    let plan = call(client, "render_plan", args).await.expect("render_plan");
    let image = plan.content.iter().find_map(|c| c.as_image()).expect("image content");
    let png = STANDARD.decode(&image.data).expect("base64");
    Drawn {
        pixmap: Pixmap::decode_png(&png).expect("a PNG"),
        svg: svg_of(&plan).expect("svg"),
    }
}

#[tokio::test]
async fn openings_cut_the_walls_they_cross_and_draw_their_symbol() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (site, floor) = site_and_floor(&rw, "Openings").await;
    let other_floor = ok(
        &rw,
        "upsert_floor",
        json!({"site_id": site, "name": "Upper", "level": 1, "reason": "test"}),
    )
    .await["id"]
        .clone();
    let dxf = walled_dxf();
    let file = ok(
        &rw,
        "upload_file",
        json!({"name": "walls.dxf", "base64": STANDARD.encode(&dxf), "reason": "test"}),
    )
    .await;
    ok(
        &rw,
        "import_dxf",
        json!({"file_sha256": file["sha256"], "floor_id": floor, "reason": "test"}),
    )
    .await;
    let plain = json!({"site_id": site, "floor_id": floor});
    // Without objects drawn, only the cut shows.
    let walls_only = json!({"site_id": site, "floor_id": floor, "layers": {"objects": []}});
    let before = draw(&rw, plain.clone()).await;
    assert!(inked(&before.pixmap, 2.8, 1.5) && inked(&before.pixmap, 2.8, 1.7));

    let arch_line = json!({"type": "LineString", "coordinates": [[2.0, 1.5], [3.6, 1.5]]});
    let arch = ok(
        &rw,
        "upsert_object",
        json!({"site_id": site, "floor_id": floor, "kind": "opening", "name": "Arch", "geometry": arch_line,
               "attributes": {"opening_type": "arch"}, "reason": "test"}),
    )
    .await["id"]
        .as_i64()
        .expect("arch id");
    let with_arch = draw(&rw, plain.clone()).await;
    // The centre line and both faces are open across the arch.
    for y in [1.3, 1.5, 1.7] {
        assert!(!inked(&with_arch.pixmap, 2.8, y), "wall at y = {y} m is cut");
        assert!(
            inked(&with_arch.pixmap, 1.0, y),
            "wall at y = {y} m is kept left of the arch"
        );
        assert!(
            inked(&with_arch.pixmap, 4.6, y),
            "wall at y = {y} m is kept right of the arch"
        );
    }
    // Nothing is opened or drawn outside the arch: its width, and its jambs
    // and faces 0.3 m either side of the wall. A wall
    // drawn in two pieces may shade its edge pixels a little differently, so
    // outside the arch a pixel may change but not go blank or gain ink.
    let (left, top) = walled_px(2.0, 1.9);
    let (right, bottom) = walled_px(3.6, 1.2);
    let blank = |p: resvg::tiny_skia::PremultipliedColorU8| (p.red(), p.green(), p.blue()) == (255, 255, 255);
    for (x, y) in changed_pixels(&before.pixmap, &with_arch.pixmap) {
        let in_arch = (left - 3..=right + 3).contains(&x) && (top - 3..=bottom + 3).contains(&y);
        let (was, is) = (
            before.pixmap.pixel(x, y).expect("inside"),
            with_arch.pixmap.pixel(x, y).expect("inside"),
        );
        assert!(in_arch || blank(was) == blank(is), "opened or inked at ({x}, {y})");
    }
    // The walls end at the jambs.
    assert!(
        with_arch.svg.contains("L533.33 410.00 M928.00 410.00"),
        "centre line cut at x = 2 and 3.6 m"
    );
    assert_eq!(
        with_arch.svg.matches("<line ").count(),
        4,
        "two jamb ticks and two faces"
    );
    assert_eq!(with_arch.svg.matches("stroke-dasharray").count(), 2, "two dashed faces");
    assert!(!with_arch.svg.contains("<path class"), "no curve");
    assert!(!with_arch.svg.contains(">Arch</text>"), "openings have no label");
    let highlighted = draw(
        &rw,
        json!({"site_id": site, "floor_id": floor, "highlight": [{"object": arch}]}),
    )
    .await;
    assert_eq!(highlighted.svg.matches(r##"stroke="#e8590c""##).count(), 4);

    // A plain opening has its ticks and no curve.
    let set = async |attributes: Value| {
        ok(
            &rw,
            "set_attributes",
            json!({"kind": "object", "id": arch, "set": attributes, "reason": "test"}),
        )
        .await;
    };
    set(json!({"opening_type": "opening"})).await;
    let opening = draw(&rw, plain.clone()).await;
    assert_eq!(opening.svg.matches("<line ").count(), 2);
    assert_eq!(opening.svg.matches("stroke-dasharray").count(), 0);

    // A wall 0.3 m deep cuts the centre line and leaves the faces 0.2 m out.
    set(json!({"wall_depth": 0.3})).await;
    let thin = draw(&rw, walls_only.clone()).await;
    assert!(!inked(&thin.pixmap, 2.8, 1.5));
    assert!(inked(&thin.pixmap, 2.8, 1.3) && inked(&thin.pixmap, 2.8, 1.7));
    set(json!({"wall_depth": "deep"})).await;
    let defaulted = draw(&rw, walls_only.clone()).await;
    assert!(!inked(&defaulted.pixmap, 2.8, 1.3) && !inked(&defaulted.pixmap, 2.8, 1.7));

    // A soft deleted opening cuts nothing.
    ok(&rw, "delete", json!({"kind": "object", "id": arch, "reason": "test"})).await;
    let deleted = draw(&rw, plain.clone()).await;
    assert!(changed_pixels(&before.pixmap, &deleted.pixmap).is_empty());

    // Three points are not an opening: drawn as an object, and the wall
    // stays whole.
    let bent = ok(
        &rw,
        "upsert_object",
        json!({"site_id": site, "floor_id": floor, "kind": "opening", "name": "Bent",
               "geometry": {"type": "LineString", "coordinates": [[2.0, 1.5], [2.8, 1.5], [3.6, 1.5]]},
               "attributes": {"opening_type": "arch"}, "reason": "test"}),
    )
    .await["id"]
        .clone();
    let drawn = draw(&rw, plain.clone()).await;
    assert_eq!(drawn.svg.matches("<line ").count(), 0);
    assert!(drawn.svg.contains(r#"stroke="hsl("#), "drawn as an object");
    assert!(inked(&draw(&rw, walls_only.clone()).await.pixmap, 2.8, 1.5));
    ok(&rw, "delete", json!({"kind": "object", "id": bent, "reason": "test"})).await;

    // An opening on another floor cuts nothing on this one.
    ok(
        &rw,
        "upsert_object",
        json!({"site_id": site, "floor_id": other_floor, "kind": "opening", "name": "Upstairs",
               "geometry": {"type": "LineString", "coordinates": [[2.0, 1.5], [3.6, 1.5]]}, "reason": "test"}),
    )
    .await;
    let elsewhere = draw(&rw, plain).await;
    assert!(changed_pixels(&before.pixmap, &elsewhere.pixmap).is_empty());
    assert_eq!(elsewhere.svg.matches("<line ").count(), 0);
}

/// The pixels that differ between two renders of the same size.
fn changed_pixels(a: &Pixmap, b: &Pixmap) -> Vec<(u32, u32)> {
    assert_eq!((a.width(), a.height()), (b.width(), b.height()));
    (0..a.height())
        .flat_map(|y| (0..a.width()).map(move |x| (x, y)))
        .filter(|&(x, y)| a.pixel(x, y) != b.pixel(x, y))
        .collect()
}

/// A closed square ring `side` metres on a side from (x, y).
fn ring(x: f64, y: f64, w: f64, h: f64) -> Value {
    json!({"type": "Polygon", "coordinates": [[[x, y], [x + w, y], [x + w, y + h], [x, y + h], [x, y]]]})
}

async fn id_of(value: Value) -> i64 {
    value["id"].as_i64().expect("an id")
}

#[tokio::test]
async fn partial_updates_keep_what_is_left_out() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (site, floor) = site_and_floor(&rw, "Partial").await;
    let room = id_of(
        ok(
            &rw,
            "upsert_space",
            json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Studio",
                   "geometry": ring(0.0, 0.0, 4.0, 3.0), "wall_material": "stone", "reason": "test"}),
        )
        .await,
    )
    .await;
    let lamp = id_of(
        ok(
            &rw,
            "upsert_object",
            json!({"site_id": site, "floor_id": floor, "kind": "light", "name": "Lamp", "height": 2.4,
                   "geometry": {"type": "Point", "coordinates": [1.0, 2.0]}, "attributes": {"watts": 9}, "reason": "test"}),
        )
        .await,
    )
    .await;
    let before = ok(&rw, "get_floor", json!({"floor_id": floor})).await;

    // A rename by id alone keeps the geometry, kind, floor and the rest.
    ok(
        &rw,
        "upsert_object",
        json!({"id": lamp, "site_id": site, "name": "Reading lamp", "reason": "renamed"}),
    )
    .await;
    ok(
        &rw,
        "upsert_space",
        json!({"id": room, "site_id": site, "name": "Den", "reason": "renamed"}),
    )
    .await;
    let after = ok(&rw, "get_floor", json!({"floor_id": floor})).await;
    let (old_lamp, new_lamp) = (&before["objects"][0], &after["objects"][0]);
    assert_eq!(new_lamp["name"], "Reading lamp");
    for key in ["geometry", "kind", "floor_id", "attributes", "refs"] {
        assert_eq!(new_lamp[key], old_lamp[key], "object {key} kept");
    }
    let (old_room, new_room) = (&before["spaces"][0], &after["spaces"][0]);
    assert_eq!(new_room["name"], "Den");
    for key in ["geometry", "kind", "wall_material", "attributes"] {
        assert_eq!(new_room[key], old_room[key], "space {key} kept");
    }
    let found = ok(&rw, "find", json!({"site_id": site, "name": "Reading"})).await;
    assert_eq!(found["objects"][0]["floor_id"], floor);

    // New geometry alone keeps the name; null clears what may be cleared.
    ok(
        &rw,
        "upsert_object",
        json!({"id": lamp, "site_id": site, "geometry": {"type": "Point", "coordinates": [3.0, 1.0]},
               "height": null, "reason": "moved"}),
    )
    .await;
    let history = ok(&rw, "history", json!({"kind": "object", "id": lamp, "limit": 1})).await;
    assert_eq!(history[0]["after"]["name"], "Reading lamp");
    assert_eq!(history[0]["after"]["height"], Value::Null);
    assert_eq!(history[0]["after"]["floor_id"], floor);
    assert_eq!(history[0]["reason"], "moved");

    // On create the three are still required, and the error names the one
    // missing.
    let missing = call(
        &rw,
        "upsert_object",
        json!({"site_id": site, "floor_id": floor, "kind": "light", "name": "No shape", "reason": "test"}),
    )
    .await
    .expect_err("geometry is required on create");
    assert!(
        missing.contains("geometry") && missing.contains("required"),
        "{missing}"
    );
    let missing = call(
        &rw,
        "upsert_space",
        json!({"site_id": site, "kind": "room", "geometry": ring(5.0, 0.0, 1.0, 1.0), "reason": "test"}),
    )
    .await
    .expect_err("name is required on create");
    assert!(missing.contains("name"), "{missing}");
    let missing = call(
        &rw,
        "upsert_space",
        json!({"site_id": site, "name": "Kindless", "geometry": ring(5.0, 0.0, 1.0, 1.0), "reason": "test"}),
    )
    .await
    .expect_err("kind is required on create");
    assert!(missing.contains("kind"), "{missing}");
}

/// Three 3 by 3 m rooms in a row on one floor, a fourth apart from them, a
/// door between the first two and an arch between the second and third,
/// each across a wall drawn as the rooms' shared edge.
async fn three_rooms(rw: &RunningService<RoleClient, ()>, site: i64, floor: i64) -> ([i64; 4], i64, i64) {
    let mut rooms = [0; 4];
    for (i, x) in [0.0, 3.0, 6.0, 20.0].into_iter().enumerate() {
        rooms[i] = id_of(
            ok(
                rw,
                "upsert_space",
                json!({"site_id": site, "floor_id": floor, "kind": "room", "name": format!("Room {}", i + 1),
                       "geometry": ring(x, 0.0, 3.0, 3.0), "reason": "test"}),
            )
            .await,
        )
        .await;
    }
    let opening = async |name: &str, x: f64, attributes: Value| {
        id_of(
            ok(
                rw,
                "upsert_object",
                json!({"site_id": site, "floor_id": floor, "kind": "opening", "name": name,
                       "geometry": {"type": "LineString", "coordinates": [[x, 1.0], [x, 2.0]]},
                       "attributes": attributes, "reason": "test"}),
            )
            .await,
        )
        .await
    };
    let door = opening(
        "Door",
        3.0,
        json!({"opening_type": "door", "hinge": "end", "swing": "right", "wall_depth": 0.3}),
    )
    .await;
    let arch = opening("Arch", 6.0, json!({"opening_type": "arch", "wall_depth": 0.3})).await;
    (rooms, door, arch)
}

#[tokio::test]
async fn connections_lists_neighbours_and_finds_the_shortest_path() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (site, floor) = site_and_floor(&rw, "Connections").await;
    let ([one, two, three, four], door, arch) = three_rooms(&rw, site, floor).await;
    // A building outline round them all is never the room beyond a door.
    ok(
        &rw,
        "upsert_space",
        json!({"site_id": site, "floor_id": floor, "kind": "building", "name": "House",
               "geometry": ring(-1.0, -1.0, 11.0, 5.0), "reason": "test"}),
    )
    .await;

    // The middle room's neighbours, from geometry, with the read only token.
    let middle = ok(&ro, "connections", json!({"space_id": two})).await;
    assert_eq!(middle["space"]["id"], two);
    let mut found: Vec<(i64, String, i64)> = middle["connections"]
        .as_array()
        .expect("connections")
        .iter()
        .map(|c| {
            (
                c["opening"]["id"].as_i64().expect("opening"),
                c["opening"]["opening_type"].as_str().expect("type").to_owned(),
                c["other_side"]["id"].as_i64().expect("other side"),
            )
        })
        .collect();
    found.sort();
    assert_eq!(found, [(door, "door".into(), one), (arch, "arch".into(), three)]);
    assert!(
        middle["connections"]
            .as_array()
            .expect("list")
            .iter()
            .all(|c| c["from"] == "geometry")
    );
    let end = ok(&ro, "connections", json!({"space_id": one})).await;
    assert_eq!(end["connections"].as_array().expect("list").len(), 1);
    assert_eq!(end["connections"][0]["other_side"]["id"], two);

    // The way from the first room to the third: through the door and the arch.
    let way = ok(&ro, "connections", json!({"from_space": one, "to_space": three})).await;
    assert_eq!(way["openings_passed"], 2);
    let steps: Vec<Value> = way["path"]
        .as_array()
        .expect("a path")
        .iter()
        .map(|step| match (step.get("space"), step.get("opening")) {
            (Some(space), None) => json!({"space": space["id"]}),
            (None, Some(opening)) => json!({"opening": opening["id"]}),
            _ => panic!("a step is a space or an opening: {step}"),
        })
        .collect();
    assert_eq!(
        steps,
        [
            json!({"space": one}),
            json!({"opening": door}),
            json!({"space": two}),
            json!({"opening": arch}),
            json!({"space": three})
        ]
    );

    // No way into the room apart, and saying so plainly.
    let none = ok(&ro, "connections", json!({"from_space": one, "to_space": four})).await;
    assert_eq!(none["path"], Value::Null);
    assert_eq!(
        none["message"],
        "No path from Room 1 to Room 4 through recorded openings."
    );

    // `connects` names the spaces outright, and a bad one is reported and
    // passed over.
    ok(
        &rw,
        "set_attributes",
        json!({"kind": "object", "id": door, "set": {"connects": [one, four]}, "reason": "test"}),
    )
    .await;
    let way = ok(&ro, "connections", json!({"from_space": one, "to_space": four})).await;
    assert_eq!(way["openings_passed"], 1);
    ok(
        &rw,
        "set_attributes",
        json!({"kind": "object", "id": door, "set": {"connects": [one, 999_999]}, "reason": "test"}),
    )
    .await;
    let middle = ok(&ro, "connections", json!({"space_id": two})).await;
    assert_eq!(middle["connections"].as_array().expect("list").len(), 2);
    assert!(middle["warnings"][0].as_str().expect("a warning").contains("999999"));

    // Asking wrongly names the field.
    let error = call(&ro, "connections", json!({"from_space": one}))
        .await
        .expect_err("needs to_space");
    assert!(error.contains("to_space"), "{error}");
    let error = call(&ro, "connections", json!({"space_id": 999_999}))
        .await
        .expect_err("no space");
    assert!(error.contains("space_id"), "{error}");
}

#[tokio::test]
async fn set_site_origin_places_a_site_and_is_recorded() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let site = id_of(ok(&rw, "create_site", json!({"name": "Unplaced", "reason": "test"})).await).await;
    let square =
        json!({"type": "Feature", "properties": {}, "geometry": {"type": "Polygon", "coordinates": square(9.0)}});
    let refused = call(
        &rw,
        "import_geojson",
        json!({"site_id": site, "geojson": square, "as": "space", "kind": "parcel", "reason": "test"}),
    )
    .await
    .expect_err("no georeference yet");
    assert!(refused.contains("georeference"), "{refused}");

    let set = ok(
        &rw,
        "set_site_origin",
        json!({"site_id": site, "origin": {"epsg": 4326, "x": 9.0, "y": 45.0}, "rotation": 10.0, "reason": "origin from the survey"}),
    )
    .await;
    assert!((set["origin_wgs84"]["lon"].as_f64().expect("lon") - 9.0).abs() < 1e-9);
    assert!((set["origin_wgs84"]["lat"].as_f64().expect("lat") - 45.0).abs() < 1e-9);
    assert_eq!(set["rotation"], 10.0);
    let sites = ok(&ro, "list_sites", json!({})).await;
    assert_eq!(sites[0]["georeferenced"], true);
    assert_eq!(sites[0]["rotation"], 10.0);
    // Now geographic input lands in the frame.
    ok(
        &rw,
        "import_geojson",
        json!({"site_id": site, "geojson": square, "as": "space", "kind": "parcel", "reason": "test"}),
    )
    .await;

    // Moved again, the rotation left out stays; both writes are in history.
    let moved = ok(
        &rw,
        "set_site_origin",
        json!({"site_id": site, "origin": {"epsg": 32632, "x": 500000.0, "y": 5000000.0}, "reason": "moved the origin"}),
    )
    .await;
    assert_eq!(moved["rotation"], 10.0);
    let history = ok(&ro, "history", json!({"kind": "site", "id": site})).await;
    let reasons: Vec<Value> = column(&history, "reason");
    assert_eq!(
        &reasons[..2],
        [json!("moved the origin"), json!("origin from the survey")]
    );
    assert_eq!(history[0]["author"], "jesse");

    // Refusals name their field, and the read only token cannot set it.
    for (args, field) in [
        (
            json!({"site_id": site, "origin": {"epsg": 999_999, "x": 0.0, "y": 0.0}, "reason": "test"}),
            "origin.epsg",
        ),
        (
            json!({"site_id": site, "origin": {"epsg": 4326, "x": -120.0, "y": 45.0}, "reason": "test"}),
            "origin",
        ),
        (
            json!({"site_id": site, "origin": {"epsg": 4326, "x": 9.0, "y": 45.0}, "rotation": 400.0, "reason": "test"}),
            "rotation",
        ),
        (
            json!({"site_id": 999_999, "origin": {"epsg": 4326, "x": 9.0, "y": 45.0}, "reason": "test"}),
            "site_id",
        ),
    ] {
        let error = call(&rw, "set_site_origin", args).await.expect_err("refused");
        assert!(
            error.starts_with(&format!("{field}:")) || error.contains(&format!("{field}:")),
            "{error}"
        );
    }
    let error = call(
        &ro,
        "set_site_origin",
        json!({"site_id": site, "origin": {"epsg": 4326, "x": 9.0, "y": 45.0}, "reason": "test"}),
    )
    .await
    .expect_err("read only");
    assert!(error.contains("read and write token"), "{error}");
}

#[tokio::test]
async fn what_is_at_without_a_floor_searches_outdoors_and_every_floor() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (site, ground) = site_and_floor(&rw, "Floors").await;
    let upper = id_of(
        ok(
            &rw,
            "upsert_floor",
            json!({"site_id": site, "name": "Upper", "level": 1, "reason": "test"}),
        )
        .await,
    )
    .await;
    for (floor, name) in [(Some(ground), "Kitchen"), (Some(upper), "Bedroom"), (None, "Garden")] {
        let (x, side) = if floor.is_some() { (0.0, 4.0) } else { (-10.0, 30.0) };
        ok(
            &rw,
            "upsert_space",
            json!({"site_id": site, "floor_id": floor, "kind": if floor.is_some() { "room" } else { "garden_bed" },
                   "name": name, "geometry": ring(x, if floor.is_some() { 0.0 } else { -10.0 }, side, side), "reason": "test"}),
        )
        .await;
    }
    ok(
        &rw,
        "upsert_object",
        json!({"site_id": site, "floor_id": upper, "kind": "light", "name": "Upstairs light",
               "geometry": {"type": "Point", "coordinates": [2.0, 2.2]}, "reason": "test"}),
    )
    .await;
    let everywhere = ok(&rw, "what_is_at", json!({"site_id": site, "x": 2.0, "y": 2.0})).await;
    assert_eq!(everywhere["searched"], "outdoors and every floor");
    let hits: Vec<(String, Value)> = everywhere["spaces"]
        .as_array()
        .expect("spaces")
        .iter()
        .map(|s| (s["name"].as_str().expect("name").to_owned(), s["floor"]["name"].clone()))
        .collect();
    assert_eq!(
        hits,
        [
            ("Garden".into(), Value::Null),
            ("Kitchen".into(), json!("Ground")),
            ("Bedroom".into(), json!("Upper"))
        ]
    );
    assert_eq!(everywhere["objects"][0]["floor"]["level"], 1);
    let ground_only = ok(
        &rw,
        "what_is_at",
        json!({"site_id": site, "floor_id": ground, "x": 2.0, "y": 2.0}),
    )
    .await;
    assert_eq!(sorted_names(&ground_only, "spaces"), ["Garden", "Kitchen"]);
    assert_eq!(ground_only["objects"], json!([]));
}

#[tokio::test]
async fn sites_and_floors_keep_attributes() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (site, floor) = site_and_floor(&rw, "Attributed").await;
    let sites = ok(&ro, "list_sites", json!({})).await;
    assert_eq!(sites[0]["attributes"], json!({}));
    assert_eq!(sites[0]["floors"][0]["attributes"], json!({}));
    let set = ok(
        &rw,
        "set_attributes",
        json!({"kind": "site", "id": site, "set": {"survey": "sheet 1", "scale": 100}, "reason": "provenance"}),
    )
    .await;
    assert_eq!(set["attributes"], json!({"survey": "sheet 1", "scale": 100}));
    ok(
        &rw,
        "set_attributes",
        json!({"kind": "site", "id": site, "remove": ["scale"], "reason": "test"}),
    )
    .await;
    ok(
        &rw,
        "set_attributes",
        json!({"kind": "floor", "id": floor, "set": {"source_drawing": "sheet 2"}, "reason": "provenance"}),
    )
    .await;
    let sites = ok(&ro, "list_sites", json!({})).await;
    assert_eq!(sites[0]["attributes"], json!({"survey": "sheet 1"}));
    assert_eq!(
        sites[0]["floors"][0]["attributes"],
        json!({"source_drawing": "sheet 2"})
    );
    let got = ok(&ro, "get_floor", json!({"floor_id": floor})).await;
    assert_eq!(got["attributes"], json!({"source_drawing": "sheet 2"}));
    let history = ok(&ro, "history", json!({"kind": "floor", "id": floor, "limit": 1})).await;
    assert_eq!(history[0]["reason"], "provenance");
    let error = call(
        &rw,
        "set_attributes",
        json!({"kind": "floor", "id": 999_999, "set": {"a": 1}, "reason": "test"}),
    )
    .await
    .expect_err("no floor");
    assert!(error.contains("id"), "{error}");
}

#[tokio::test]
async fn a_drawing_of_the_grounds_imports_onto_the_site() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (site, floor) = site_and_floor(&rw, "Grounds").await;
    let dxf = two_room_dxf();
    let file = ok(
        &rw,
        "upload_file",
        json!({"name": "grounds.dxf", "base64": STANDARD.encode(&dxf), "reason": "test"}),
    )
    .await;
    let layers = json!({"A-ROOM": {"space": "terrace"}, "E-LIGHT": {"object": "light"}});
    let import = ok(
        &rw,
        "import_dxf",
        json!({"file_sha256": file["sha256"], "site_id": site, "layers": layers, "reason": "grounds"}),
    )
    .await;
    assert_eq!(import["linework"], 5);
    assert_eq!(import["replaced_import_id"], Value::Null);
    // Its spaces and objects are outdoor ones.
    let found = ok(&rw, "find", json!({"site_id": site, "kind": "terrace"})).await;
    assert_eq!(column(&found["spaces"], "floor_id"), [Value::Null, Value::Null]);
    let outdoors = ok(&rw, "what_is_at", json!({"site_id": site, "x": 1.0, "y": 1.0})).await;
    assert_eq!(outdoors["spaces"][0]["floor"], Value::Null);

    // The site plan draws its linework; the floor's plan does not.
    let site_plan = draw(&rw, json!({"site_id": site})).await;
    assert!(site_plan.svg.contains(r##"stroke="#8a8a8a""##), "linework drawn");
    let linework_only = draw(
        &rw,
        json!({"site_id": site, "layers": {"spaces": false, "objects": [], "labels": false}}),
    )
    .await;
    assert!(linework_only.svg.matches(r##"stroke="#8a8a8a""##).count() >= 2);
    ok(
        &rw,
        "upsert_space",
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Hall", "geometry": ring(0.0, 0.0, 2.0, 2.0), "reason": "test"}),
    )
    .await;
    let floor_plan = draw(&rw, json!({"site_id": site, "floor_id": floor})).await;
    assert!(
        !floor_plan.svg.contains(r##"stroke="#8a8a8a""##),
        "no site linework on a floor"
    );

    // A second import of the same drawing onto the site replaces the first.
    let again = ok(
        &rw,
        "import_dxf",
        json!({"file_sha256": file["sha256"], "site_id": site, "layers": layers, "reason": "again"}),
    )
    .await;
    assert_eq!(again["replaced_import_id"], import["import_id"]);
    let found = ok(&rw, "find", json!({"site_id": site, "kind": "terrace"})).await;
    assert_eq!(found["spaces"].as_array().expect("spaces").len(), 2);

    // Neither a floor nor a site, or a site that is not the floor's, is refused.
    let error = call(
        &rw,
        "import_dxf",
        json!({"file_sha256": file["sha256"], "reason": "test"}),
    )
    .await
    .expect_err("no place");
    assert!(error.contains("floor_id:"), "{error}");
    let other = id_of(ok(&rw, "create_site", json!({"name": "Elsewhere", "reason": "test"})).await).await;
    let error = call(
        &rw,
        "import_dxf",
        json!({"file_sha256": file["sha256"], "floor_id": floor, "site_id": other, "reason": "test"}),
    )
    .await
    .expect_err("wrong site");
    assert!(error.contains("site_id:"), "{error}");
}

#[tokio::test]
async fn a_scratch_floor_draws_its_door_arch_and_crowded_labels() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (site, floor) = site_and_floor(&rw, "Drawn").await;
    let _ = three_rooms(&rw, site, floor).await;
    // A small room crowded with five labelled objects.
    ok(
        &rw,
        "upsert_space",
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Alcove",
               "geometry": ring(9.0, 0.0, 1.2, 1.2), "reason": "test"}),
    )
    .await;
    for i in 0..5 {
        let (x, y) = (9.2 + 0.18 * f64::from(i), 0.3 + 0.15 * f64::from(i));
        ok(
            &rw,
            "upsert_object",
            json!({"site_id": site, "floor_id": floor, "kind": "furniture", "name": format!("Piece {i}"),
                   "geometry": {"type": "Point", "coordinates": [x, y]}, "reason": "test"}),
        )
        .await;
    }
    for size in ["normal", "large"] {
        let drawn = draw(&rw, json!({"site_id": site, "floor_id": floor, "label_size": size})).await;
        assert_eq!(drawn.svg.matches(r#"class="door-leaf""#).count(), 1);
        assert_eq!(drawn.svg.matches(r#"class="door-swing""#).count(), 1);
        assert_eq!(drawn.svg.matches(r#"class="arch-face""#).count(), 2);
        for name in ["Room 1", "Room 2", "Room 3", "Alcove", "Piece 0", "Piece 4"] {
            assert!(drawn.svg.contains(&format!(">{name}</text>")), "{size}: {name}");
        }
    }
    let refused = call(
        &rw,
        "render_plan",
        json!({"site_id": site, "floor_id": floor, "label_size": "huge"}),
    )
    .await
    .expect("a tool result");
    assert_eq!(refused.is_error, Some(true));
    let text = &refused.content[0].as_text().expect("text").text;
    assert!(text.contains("huge") && text.contains("large"), "{text}");
}

/// A floor shaped like a real one: 25 rooms of irregular outline, objects
/// in them, and an update by id sent with everything get_floor returned for
/// it, as a caller that read the floor and changed one field would send.
#[tokio::test]
async fn upsert_floor_takes_a_real_floors_payload() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (site, floor) = site_and_floor(&rw, "Payload").await;
    for i in 0..25 {
        let (x, y) = (f64::from(i % 5) * 4.0, f64::from(i / 5) * 4.0);
        // An outline of twelve vertices, a room with a chimney breast.
        let outline = json!({"type": "Polygon", "coordinates": [[
            [x, y], [x + 1.5, y], [x + 1.5, y + 0.3], [x + 2.1, y + 0.3], [x + 2.1, y], [x + 3.8, y],
            [x + 3.8, y + 3.8], [x + 2.0, y + 3.8], [x + 2.0, y + 3.5], [x + 1.0, y + 3.5], [x + 1.0, y + 3.8],
            [x, y + 3.8], [x, y]
        ]]});
        ok(
            &rw,
            "upsert_space",
            json!({"site_id": site, "floor_id": floor, "kind": "room", "name": format!("Stanza {i}"),
                   "geometry": outline, "attributes": {"name_en": format!("Room {i}"), "area_m2": 13.5}, "reason": "test"}),
        )
        .await;
        ok(
            &rw,
            "upsert_object",
            json!({"site_id": site, "floor_id": floor, "kind": "light", "name": format!("Light {i}"),
                   "geometry": {"type": "Point", "coordinates": [x + 1.9, y + 1.9]}, "reason": "test"}),
        )
        .await;
    }
    let mut payload = ok(&rw, "get_floor", json!({"floor_id": floor})).await;
    assert_eq!(payload["spaces"].as_array().expect("spaces").len(), 25);
    for round in 0..20 {
        payload["name"] = json!(format!("Ground, round {round}"));
        payload["ceiling_height"] = json!(2.7);
        payload["reason"] = json!("renamed with the whole floor sent back");
        payload["id"] = json!(floor);
        let updated = ok(&rw, "upsert_floor", payload.clone()).await;
        assert_eq!(updated["id"], floor);
    }
    let after = ok(&rw, "get_floor", json!({"floor_id": floor})).await;
    assert_eq!(after["name"], "Ground, round 19");
    assert_eq!(after["spaces"], payload["spaces"], "the rooms are untouched");
    // Sixteen at once from separate clients, twice the pool's connections,
    // as parallel callers would send them.
    let mut calls = tokio::task::JoinSet::new();
    for i in 0..16 {
        let mut payload = payload.clone();
        payload["name"] = json!(format!("Parallel {i}"));
        let caller = client(&server, RW).await;
        calls.spawn(async move { call(&caller, "upsert_floor", payload).await });
    }
    while let Some(result) = calls.join_next().await {
        result.expect("a task").expect("upsert_floor");
    }
}

/// A point geometry.
fn point(x: f64, y: f64) -> Value {
    json!({"type": "Point", "coordinates": [x, y]})
}

/// Creates an object and returns its id.
async fn object(rw: &RunningService<RoleClient, ()>, mut args: Value) -> i64 {
    args["reason"] = json!("test");
    id_of(ok(rw, "upsert_object", args).await).await
}

/// Creates a space and returns its id.
async fn space(rw: &RunningService<RoleClient, ()>, mut args: Value) -> i64 {
    args["reason"] = json!("test");
    id_of(ok(rw, "upsert_space", args).await).await
}

#[tokio::test]
async fn a_deleted_site_or_floor_hides_everything_it_holds() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (site, floor) = site_and_floor(&rw, "Hidden").await;
    let room = space(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Hidden room", "geometry": ring(0.0, 0.0, 4.0, 3.0)}),
    )
    .await;
    let hall = space(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Hidden hall", "geometry": ring(4.0, 0.0, 2.0, 3.0)}),
    )
    .await;
    let lamp = object(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "light", "name": "Hidden lamp", "geometry": point(1.0, 1.0)}),
    )
    .await;
    object(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "opening", "name": "Hidden door",
               "geometry": {"type": "LineString", "coordinates": [[4.0, 1.0], [4.0, 2.0]]}, "attributes": {"connects": [room, hall]}}),
    )
    .await;
    let garden = space(
        &rw,
        json!({"site_id": site, "kind": "garden_bed", "name": "Hidden bed", "geometry": ring(0.0, 10.0, 2.0, 2.0)}),
    )
    .await;
    let tree = object(
        &rw,
        json!({"site_id": site, "kind": "tree", "name": "Hidden tree", "geometry": point(1.0, 11.0)}),
    )
    .await;

    // Everything the read tools can reach, as (what, visible).
    async fn visible(
        ro: &RunningService<RoleClient, ()>,
        site: i64,
        floor: i64,
        room: i64,
        lamp: i64,
    ) -> Vec<(&'static str, bool)> {
        let found = ok(ro, "find", json!({"name": "hidden"})).await;
        let at = ok(
            ro,
            "what_is_at",
            json!({"site_id": site, "x": 1.0, "y": 1.0, "radius": 1.0}),
        )
        .await;
        let outdoors = ok(
            ro,
            "what_is_at",
            json!({"site_id": site, "x": 1.0, "y": 11.0, "radius": 1.0}),
        )
        .await;
        let nearest = ok(
            ro,
            "nearest",
            json!({"site_id": site, "kind": "light", "from": {"point": [0.0, 0.0]}}),
        )
        .await;
        let sites = ok(ro, "list_sites", json!({})).await;
        vec![
            ("find spaces", !found["spaces"].as_array().expect("spaces").is_empty()),
            (
                "find objects",
                !found["objects"].as_array().expect("objects").is_empty(),
            ),
            (
                "what_is_at spaces",
                !at["spaces"].as_array().expect("spaces").is_empty(),
            ),
            (
                "what_is_at objects",
                !at["objects"].as_array().expect("objects").is_empty(),
            ),
            (
                "what_is_at outdoors",
                !outdoors["objects"].as_array().expect("objects").is_empty(),
            ),
            ("nearest", !nearest["objects"].as_array().expect("objects").is_empty()),
            (
                "in_space",
                call(ro, "in_space", json!({"space_id": room})).await.is_ok(),
            ),
            (
                "measure space",
                call(ro, "measure", json!({"a": {"space": room}})).await.is_ok(),
            ),
            (
                "measure object",
                call(ro, "measure", json!({"a": {"object": lamp}})).await.is_ok(),
            ),
            (
                "connections",
                call(ro, "connections", json!({"space_id": room})).await.is_ok(),
            ),
            (
                "get_floor",
                call(ro, "get_floor", json!({"floor_id": floor})).await.is_ok(),
            ),
            (
                "render_plan",
                call(ro, "render_plan", json!({"site_id": site, "floor_id": floor}))
                    .await
                    .is_ok(),
            ),
            ("list_sites", !sites.as_array().expect("sites").is_empty()),
        ]
    }
    let all = |seen: Vec<(&'static str, bool)>, expected: bool| {
        for (what, shown) in seen {
            assert_eq!(shown, expected, "{what}");
        }
    };
    all(visible(&ro, site, floor, room, lamp).await, true);

    // Deleting the site marks only the site, and hides all it holds.
    let deleted = ok(&rw, "delete", json!({"kind": "site", "id": site, "reason": "test"})).await;
    assert_eq!(deleted["hides"], json!({"floors": 1, "spaces": 3, "objects": 3}));
    all(visible(&ro, site, floor, room, lamp).await, false);
    let history = ok(&ro, "history", json!({"kind": "space", "id": room})).await;
    assert_eq!(
        history.as_array().expect("history").len(),
        1,
        "the room was not written"
    );

    // Restoring it brings everything back as it was.
    ok(&rw, "restore", json!({"kind": "site", "id": site, "reason": "test"})).await;
    all(visible(&ro, site, floor, room, lamp).await, true);

    // A deleted floor hides its spaces and objects, but not the outdoor ones.
    let deleted = ok(&rw, "delete", json!({"kind": "floor", "id": floor, "reason": "test"})).await;
    assert_eq!(deleted["hides"], json!({"floors": 1, "spaces": 2, "objects": 2}));
    let found = ok(&ro, "find", json!({"name": "hidden"})).await;
    assert_eq!(column(&found["spaces"], "id"), [json!(garden)]);
    assert_eq!(column(&found["objects"], "id"), [json!(tree)]);
    let at = ok(
        &ro,
        "what_is_at",
        json!({"site_id": site, "x": 1.0, "y": 1.0, "radius": 1.0}),
    )
    .await;
    assert_eq!((at["spaces"].clone(), at["objects"].clone()), (json!([]), json!([])));
    assert!(call(&ro, "in_space", json!({"space_id": room})).await.is_err());
    assert!(call(&ro, "connections", json!({"space_id": room})).await.is_err());
    let nearest = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "light", "from": {"point": [0.0, 0.0]}}),
    )
    .await;
    assert_eq!(nearest["objects"], json!([]));
    ok(&rw, "restore", json!({"kind": "floor", "id": floor, "reason": "test"})).await;
    all(visible(&ro, site, floor, room, lamp).await, true);

    // A row deleted on its own stays deleted when its site comes back.
    ok(&rw, "delete", json!({"kind": "object", "id": lamp, "reason": "test"})).await;
    ok(&rw, "delete", json!({"kind": "site", "id": site, "reason": "test"})).await;
    ok(&rw, "restore", json!({"kind": "site", "id": site, "reason": "test"})).await;
    let found = ok(&ro, "find", json!({"name": "hidden lamp"})).await;
    assert_eq!(found["objects"], json!([]));
}

#[tokio::test]
async fn find_matches_every_name_and_ranks_the_best_first() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (site, floor) = site_and_floor(&rw, "Names").await;
    let cambusa = space(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Cambusa", "geometry": ring(0.0, 0.0, 6.0, 4.0),
               "attributes": {"common_name": "the scullery"}}),
    )
    .await;
    let stiva = space(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Stiva", "geometry": ring(4.0, 0.0, 2.0, 2.0)}),
    )
    .await;
    let snug = space(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Salottino", "geometry": ring(6.0, 0.0, 5.0, 4.0),
               "attributes": {"nicknames": ["snug", 3, "den"]}}),
    )
    .await;
    let zaffiro = space(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Sala Zàffiro", "geometry": ring(11.0, 0.0, 2.0, 4.0)}),
    )
    .await;
    // Made so that neither id nor name order gives the ranked order: a
    // contains match, a prefix match on an outside identifier, and an exact
    // match on a nickname.
    let contains = object(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "light", "name": "A scullery lamp", "geometry": point(1.0, 1.0)}),
    )
    .await;
    let prefix = object(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "camera", "name": "Mid cam", "geometry": point(5.0, 1.0),
               "refs": {"ha_name": "Scullery Camera"}}),
    )
    .await;
    let exact = object(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "sensor", "name": "Zed", "geometry":
               {"type": "LineString", "coordinates": [[0.5, 3.0], [2.5, 3.0]]}, "attributes": {"nicknames": ["Scullery"]}}),
    )
    .await;

    let found = ok(&ro, "find", json!({"name": "scullery"})).await;
    assert_eq!(column(&found["spaces"], "id"), [json!(cambusa)]);
    assert_eq!(found["spaces"][0]["matched_on"], "common_name");
    assert_eq!(
        column(&found["objects"], "id"),
        [json!(exact), json!(prefix), json!(contains)]
    );
    assert_eq!(
        column(&found["objects"], "matched_on"),
        [json!("nicknames"), json!("refs.ha_name"), json!("name")]
    );
    // The limit applies after ranking.
    let found = ok(&ro, "find", json!({"name": "scullery", "limit": 1})).await;
    assert_eq!(column(&found["objects"], "id"), [json!(exact)]);
    // Through a nickname list, with an article in the query.
    let found = ok(&ro, "find", json!({"site_id": site, "name": "The Den"})).await;
    assert_eq!(column(&found["spaces"], "id"), [json!(snug)]);
    assert_eq!(found["spaces"][0]["matched_on"], "nicknames");
    // Without the accent, and with one where the stored name has none.
    let found = ok(&ro, "find", json!({"site_id": site, "name": "zaffiro"})).await;
    assert_eq!(column(&found["spaces"], "id"), [json!(zaffiro)]);
    let found = ok(&ro, "find", json!({"site_id": site, "name": "CAMBÙSA"})).await;
    assert_eq!(column(&found["spaces"], "id"), [json!(cambusa)]);
    assert_eq!(found["spaces"][0]["matched_on"], "name");
    // An outside identifier matches anywhere in it.
    let found = ok(&ro, "find", json!({"site_id": site, "name": "camera"})).await;
    assert_eq!(column(&found["objects"], "id"), [json!(prefix)]);
    // Nothing that does not match.
    let found = ok(&ro, "find", json!({"site_id": site, "name": "garage"})).await;
    assert_eq!(
        (found["spaces"].clone(), found["objects"].clone()),
        (json!([]), json!([]))
    );

    // Short hits carry no geometry, attributes or refs; in_space names the
    // smallest room covering the position, the midpoint for a line.
    let found = ok(&ro, "find", json!({"site_id": site, "name": "scullery"})).await;
    for hit in found["spaces"]
        .as_array()
        .into_iter()
        .chain(found["objects"].as_array())
        .flatten()
    {
        for key in ["geometry", "attributes", "refs"] {
            assert!(hit.get(key).is_none(), "{key} in {hit}");
        }
    }
    let by_id = |id: i64| {
        found["objects"]
            .as_array()
            .expect("objects")
            .iter()
            .find(|o| o["id"] == id)
            .cloned()
            .expect("hit")
    };
    assert_eq!(by_id(prefix)["in_space"], json!({"id": stiva, "name": "Stiva"}));
    assert_eq!(by_id(contains)["in_space"], json!({"id": cambusa, "name": "Cambusa"}));
    assert_eq!(by_id(exact)["in_space"]["id"], cambusa);
    assert_eq!(by_id(exact)["placement"], "exact");
    assert_eq!(found["spaces"][0]["area"], 24.0);
    // With detail, the geometry and the rest come too.
    let detailed = ok(
        &ro,
        "find",
        json!({"site_id": site, "name": "scullery", "detail": true}),
    )
    .await;
    assert_eq!(detailed["objects"][0]["geometry"]["type"], "LineString");
    assert_eq!(detailed["objects"][0]["attributes"]["nicknames"], json!(["Scullery"]));
    assert_eq!(detailed["objects"][0]["matched_on"], "nicknames");
    assert_eq!(detailed["spaces"][0]["geometry"]["type"], "Polygon");
    assert_eq!(detailed["spaces"][0]["matched_on"], "common_name");

    // in_space and nearest are short too unless asked.
    let cable = object(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "cable_run", "name": "Cable",
               "geometry": {"type": "LineString", "coordinates": [[5.0, 3.0], [7.0, 3.0]]}}),
    )
    .await;
    let inside = ok(&ro, "in_space", json!({"space_id": cambusa})).await;
    assert!(inside["objects"][0].get("geometry").is_none());
    // Only what crosses the edge says so; a short reply has no wholly_inside.
    for hit in inside["objects"].as_array().expect("objects") {
        assert!(hit.get("wholly_inside").is_none(), "{hit}");
        assert_eq!(hit.get("partly_outside").is_some(), hit["id"] == cable, "{hit}");
    }
    assert_eq!(inside["objects"].as_array().expect("objects").len(), 4);
    // Replies are not indented.
    let raw = call(&ro, "in_space", json!({"space_id": cambusa}))
        .await
        .expect("in_space");
    let text = &raw.content[0].as_text().expect("text").text;
    assert!(!text.contains('\n'), "{text}");
    let inside = ok(&ro, "in_space", json!({"space_id": cambusa, "detail": true})).await;
    assert!(inside["objects"][0].get("geometry").is_some());
    for hit in inside["objects"].as_array().expect("objects") {
        assert_eq!(hit["wholly_inside"], hit["id"] != cable, "{hit}");
    }
    let nearest = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "camera", "from": {"space": cambusa}}),
    )
    .await;
    assert!(nearest["objects"][0].get("geometry").is_none());
    assert_eq!(nearest["objects"][0]["in_space"]["id"], stiva);
    let nearest = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "camera", "from": {"space": cambusa}, "detail": true}),
    )
    .await;
    assert!(nearest["objects"][0].get("geometry").is_some());

    // An empty name is refused rather than matching everything.
    let refused = call(&ro, "find", json!({"name": "  "})).await.expect_err("refused");
    assert!(refused.contains("name: is empty"), "{refused}");
}

#[tokio::test]
async fn unplaced_objects_are_left_out_and_approximate_ones_marked() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (site, floor) = site_and_floor(&rw, "Placement").await;
    let room = space(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Study", "geometry": ring(0.0, 0.0, 4.0, 4.0)}),
    )
    .await;
    let exact = object(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "camera", "name": "Measured", "geometry": point(1.0, 1.0)}),
    )
    .await;
    let approximate = object(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "camera", "name": "Middle of the room", "geometry": point(2.0, 2.0),
               "placement": "approximate"}),
    )
    .await;
    // Its stand in position is in the room, nearer than either.
    let unplaced = object(
        &rw,
        json!({"site_id": site, "floor_id": floor, "kind": "camera", "name": "Somewhere", "geometry": point(2.1, 2.1),
               "placement": "unplaced"}),
    )
    .await;

    let nearest = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "camera", "from": {"point": [2.1, 2.1]}}),
    )
    .await;
    assert_eq!(column(&nearest["objects"], "id"), [json!(approximate), json!(exact)]);
    assert_eq!(
        column(&nearest["objects"], "placement"),
        [json!("approximate"), json!("exact")]
    );
    assert_eq!(nearest["unplaced_omitted"], 1);
    let at = ok(
        &ro,
        "what_is_at",
        json!({"site_id": site, "x": 2.1, "y": 2.1, "radius": 5.0}),
    )
    .await;
    assert_eq!(column(&at["objects"], "id"), [json!(approximate), json!(exact)]);
    assert_eq!(
        column(&at["objects"], "placement"),
        [json!("approximate"), json!("exact")]
    );
    assert_eq!(at["unplaced_omitted"], 1);
    let inside = ok(&ro, "in_space", json!({"space_id": room})).await;
    let mut ids = column(&inside["objects"], "id");
    ids.sort_by_key(|id| id.as_i64());
    assert_eq!(ids, [json!(exact), json!(approximate)]);
    assert_eq!(inside["unplaced_omitted"], 1);
    for hit in inside["objects"].as_array().expect("objects") {
        assert!(hit["placement"].is_string(), "{hit}");
    }
    // find still returns it, so that it can be placed.
    let found = ok(&ro, "find", json!({"site_id": site, "name": "somewhere"})).await;
    assert_eq!(found["objects"][0]["placement"], "unplaced");
    assert_eq!(found["objects"][0]["in_space"], Value::Null);
    // Nothing is measured from where it is not known to be.
    let refused = call(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "camera", "from": {"object": unplaced}}),
    )
    .await
    .expect_err("refused");
    assert!(refused.contains("unplaced"), "{refused}");

    // The plan leaves it out and says so, and draws the approximate one
    // hollow with a key in the legend.
    let summary = ok(&ro, "render_plan", json!({"site_id": site, "floor_id": floor})).await;
    assert_eq!(summary["not_placed"], 1);
    assert_eq!(summary["notes"], json!(["position approximate", "1 item not placed"]));
    let drawn = draw(&ro, json!({"site_id": site, "floor_id": floor, "label_from": []})).await;
    assert_eq!(drawn.svg.matches(r#"<circle class="approximate" "#).count(), 1);
    assert!(drawn.svg.contains(">1 item not placed</text>"), "count in the legend");
    assert!(drawn.svg.contains(">position approximate</text>"), "key in the legend");
    assert!(!drawn.svg.contains(">Somewhere</text>"), "unplaced not labelled");
    assert!(drawn.svg.contains(">Measured</text>"));

    // An update without placement keeps it, even with new geometry.
    ok(
        &rw,
        "upsert_object",
        json!({"id": unplaced, "site_id": site, "geometry": point(3.0, 3.0), "reason": "test"}),
    )
    .await;
    ok(
        &rw,
        "upsert_object",
        json!({"id": approximate, "site_id": site, "name": "Middle camera", "reason": "test"}),
    )
    .await;
    let found = ok(&ro, "find", json!({"site_id": site, "kind": "camera"})).await;
    let placements: Vec<(Value, Value)> = found["objects"]
        .as_array()
        .expect("objects")
        .iter()
        .map(|o| (o["id"].clone(), o["placement"].clone()))
        .collect();
    assert!(
        placements.contains(&(json!(unplaced), json!("unplaced"))),
        "{placements:?}"
    );
    assert!(
        placements.contains(&(json!(approximate), json!("approximate"))),
        "{placements:?}"
    );
    // Given, it changes.
    ok(
        &rw,
        "upsert_object",
        json!({"id": unplaced, "site_id": site, "placement": "exact", "reason": "found it"}),
    )
    .await;
    let nearest = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "camera", "from": {"point": [3.0, 3.0]}}),
    )
    .await;
    assert_eq!(nearest["objects"][0]["id"], unplaced);
    assert_eq!(nearest["unplaced_omitted"], 0);
}

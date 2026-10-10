//! The whole server over HTTP: the MCP tools through the rmcp client, and the
//! plain routes beside them.

mod common;

use base64::{Engine, engine::general_purpose::STANDARD};
use common::{TestDb, drawing, on_layer, rectangle, save, two_room_dxf};
use house_mcp::config::{Config, Token};
use resvg::tiny_skia::Pixmap;
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    service::RunningService,
    transport::{StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RW: &str = "rw-token-for-tests-0123456789abcdef0123456789";
const RO: &str = "ro-token-for-tests-0123456789abcdef0123456789";

struct Server {
    base: String,
    _files: tempfile::TempDir,
    db: TestDb,
}

async fn start() -> Server {
    let db = TestDb::new().await;
    let files = tempfile::tempdir().expect("tempdir");
    let config = Config {
        database_url: db.house_url(),
        files_dir: files.path().to_owned(),
        listen: "127.0.0.1:0".parse().expect("address"),
        allowed_hosts: vec!["127.0.0.1".into()],
        read_write: Token {
            name: "jesse".into(),
            secret: RW.into(),
        },
        read_only: Token {
            name: "readonly".into(),
            secret: RO.into(),
        },
    };
    let app = house_mcp::app(&config).await.expect("app");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    Server {
        base,
        _files: files,
        db,
    }
}

async fn client(server: &Server, token: &str) -> RunningService<RoleClient, ()> {
    let config = StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", server.base)).auth_header(token);
    ().serve(StreamableHttpClientTransport::from_config(config))
        .await
        .expect("MCP client")
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> Result<CallToolResult, String> {
    let Value::Object(args) = args else {
        panic!("arguments are an object")
    };
    client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(args))
        .await
        .map_err(|e| e.to_string())
}

async fn ok(client: &RunningService<RoleClient, ()>, tool: &'static str, args: Value) -> Value {
    let result = call(client, tool, args).await.unwrap_or_else(|e| panic!("{tool}: {e}"));
    let text = result.content.iter().find_map(|c| c.as_text()).expect("text content");
    serde_json::from_str(&text.text).unwrap_or_else(|_| Value::String(text.text.clone()))
}

/// One field of every item in a list, in order.
fn column(list: &Value, key: &str) -> Vec<Value> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|item| item[key].clone())
        .collect()
}

/// The names in `list[key]`, sorted, for lists whose order is not promised.
fn sorted_names(list: &Value, key: &str) -> Vec<String> {
    let mut names: Vec<String> = column(&list[key], "name")
        .iter()
        .map(|name| name.as_str().expect("name").to_owned())
        .collect();
    names.sort();
    names
}

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
    assert_eq!(rw_tools.len(), 24, "{rw_tools:?}");
    assert_eq!(
        ro_tools,
        [
            "find",
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
    assert_eq!(nearest[0]["id"], light);
    let distance = nearest[0]["distance"].as_f64().expect("distance");
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

    // render_plan returns a PNG at the default size and the SVG with both room names.
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
    let svg = &plan.content.iter().find_map(|c| c.as_text()).expect("svg").text;
    assert!(svg.starts_with("<svg") && svg.contains(">Kitchen</text>") && svg.contains(">Living room</text>"));

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
    assert_eq!(nearest, json!([]));
    ok(&rw, "restore", json!({"kind": "object", "id": light, "reason": "test"})).await;
    let nearest = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "light", "from": {"point": [8.0, 1.0]}}),
    )
    .await;
    assert_eq!(nearest[0]["id"], light);

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
    assert_eq!(column(&found, "id"), nearest_first);
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

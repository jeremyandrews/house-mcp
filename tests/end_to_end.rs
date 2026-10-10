//! The whole server over HTTP: the MCP tools through the rmcp client, and the
//! plain routes beside them.

mod common;

use base64::{Engine, engine::general_purpose::STANDARD};
use common::{QGIS_PASSWORD, TestDb, two_room_dxf};
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
use sqlx::{Connection, PgConnection};

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

fn names(list: &Value, key: &str) -> Vec<String> {
    let mut names: Vec<String> = list[key]
        .as_array()
        .expect("a list")
        .iter()
        .map(|v| v["name"].as_str().expect("name").to_owned())
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

    // The drawing goes in as a file, then onto the floor.
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
    assert_eq!(names(&import, "spaces"), ["Kitchen", "Living room"]);
    assert_eq!(names(&import, "objects"), ["LIGHT"]);
    assert_eq!(import["unit"], "millimetres");
    assert_eq!(import["linework"], 5);
    let light = import["objects"][0]["id"].as_i64().expect("light id");

    // What is where.
    let at = ok(
        &rw,
        "what_is_at",
        json!({"site_id": site, "floor_id": floor, "x": 1.0, "y": 1.0}),
    )
    .await;
    assert_eq!(names(&at, "spaces"), ["Kitchen"]);
    let at = ok(
        &rw,
        "what_is_at",
        json!({"site_id": site, "floor_id": floor, "x": 7.0, "y": 1.0, "radius": 0.5}),
    )
    .await;
    assert_eq!(names(&at, "spaces"), ["Living room"]);
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

    // A plan the model can look at.
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
    assert_eq!(names(&floor_now, "spaces"), ["Kitchen", "Living room"]);
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
    let mut qgis = PgConnection::connect_with(&server.db.options("house_qgis", QGIS_PASSWORD))
        .await
        .expect("qgis");
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

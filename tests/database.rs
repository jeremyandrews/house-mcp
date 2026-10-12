//! The schema on a real PostGIS: the change triggers, the role grants and the
//! site frame functions.

mod common;

use common::{RO_PASSWORD, TestDb};
use serde_json::Value;
use sqlx::{Connection, PgConnection, PgPool};

/// A site, floor, object and note written the way the server writes them.
async fn seed(pool: &PgPool) -> (i64, i64, i64) {
    let mut tx = house_mcp::db::begin_write(pool, "jesse", "seeding the test")
        .await
        .expect("begin");
    let site: i64 = sqlx::query_scalar("INSERT INTO site (name) VALUES ('test') RETURNING id")
        .fetch_one(&mut *tx)
        .await
        .expect("site");
    let floor: i64 =
        sqlx::query_scalar("INSERT INTO floor (site_id, name, level) VALUES ($1, 'Ground', 0) RETURNING id")
            .bind(site)
            .fetch_one(&mut *tx)
            .await
            .expect("floor");
    let object: i64 = sqlx::query_scalar(
        "INSERT INTO object (site_id, floor_id, kind, name, geom) VALUES ($1, $2, 'light', 'Lamp', ST_MakePoint(1, 2)) RETURNING id",
    )
    .bind(site)
    .bind(floor)
    .fetch_one(&mut *tx)
    .await
    .expect("object");
    let note: i64 =
        sqlx::query_scalar("INSERT INTO note (body, site_id) VALUES ('Fuse box in the cellar', $1) RETURNING id")
            .bind(site)
            .fetch_one(&mut *tx)
            .await
            .expect("note");
    tx.commit().await.expect("commit");
    (site, object, note)
}

#[derive(Debug, sqlx::FromRow)]
struct Change {
    author: String,
    client: String,
    reason: String,
    operation: String,
    before: Option<Value>,
    after: Option<Value>,
}

async fn changes(pool: &PgPool, table: &str, id: i64) -> Vec<Change> {
    sqlx::query_as(
        "SELECT author, client, reason, operation, before, after FROM change WHERE table_name = $1 AND row_id = $2 ORDER BY id",
    )
    .bind(table)
    .bind(id)
    .fetch_all(pool)
    .await
    .expect("changes")
}

#[tokio::test]
async fn server_writes_are_recorded_with_author_and_reason() {
    let db = TestDb::new().await;
    let pool = db.migrated_pool().await;
    let (_, object, _) = seed(&pool).await;
    let rows = changes(&pool, "object", object).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        (
            row.author.as_str(),
            row.client.as_str(),
            row.reason.as_str(),
            row.operation.as_str()
        ),
        ("jesse", "house-mcp", "seeding the test", "INSERT")
    );
    assert!(row.before.is_none());
    assert_eq!(row.after.as_ref().map(|a| &a["name"]), Some(&Value::from("Lamp")));
}

#[tokio::test]
async fn update_by_qgis_is_recorded_under_its_role_even_when_it_names_an_author() {
    let db = TestDb::new().await;
    let pool = db.migrated_pool().await;
    let (_, object, _) = seed(&pool).await;
    let mut qgis = db.qgis().await;
    sqlx::query("SELECT set_config('house.author', 'jesse', false)")
        .execute(&mut qgis)
        .await
        .expect("set");
    sqlx::query("UPDATE object SET name = 'Moved lamp', geom = ST_MakePoint(3, 4) WHERE id = $1")
        .bind(object)
        .execute(&mut qgis)
        .await
        .expect("house_qgis may update objects");
    let rows = changes(&pool, "object", object).await;
    assert_eq!(rows.len(), 2);
    let row = &rows[1];
    assert_eq!(
        (
            row.author.as_str(),
            row.client.as_str(),
            row.reason.as_str(),
            row.operation.as_str()
        ),
        ("house_qgis", "QGIS 3.40", "", "UPDATE")
    );
    let before = row.before.as_ref().expect("before");
    let after = row.after.as_ref().expect("after");
    assert_eq!(
        (&before["name"], &after["name"]),
        (&Value::from("Lamp"), &Value::from("Moved lamp"))
    );
    assert_eq!(after["geom"]["coordinates"], serde_json::json!([3, 4]));
}

#[tokio::test]
async fn hard_delete_by_qgis_keeps_the_whole_row() {
    let db = TestDb::new().await;
    let pool = db.migrated_pool().await;
    let (site, _, note) = seed(&pool).await;
    let mut qgis = db.qgis().await;
    sqlx::query("DELETE FROM note WHERE id = $1")
        .bind(note)
        .execute(&mut qgis)
        .await
        .expect("house_qgis may delete notes");
    let rows = changes(&pool, "note", note).await;
    let row = rows.last().expect("a change");
    assert_eq!((row.author.as_str(), row.operation.as_str()), ("house_qgis", "DELETE"));
    assert!(row.after.is_none());
    let before = row.before.as_ref().expect("before");
    assert_eq!(before["body"], Value::from("Fuse box in the cellar"));
    assert_eq!(before["site_id"], Value::from(site));
    assert_eq!(before["id"], Value::from(note));
}

#[tokio::test]
async fn qgis_cannot_write_change_files_or_linework() {
    let db = TestDb::new().await;
    let pool = db.migrated_pool().await;
    seed(&pool).await;
    let mut qgis = db.qgis().await;
    for sql in [
        "UPDATE change SET reason = 'rewritten'",
        "DELETE FROM change",
        "INSERT INTO change (author, client, reason, table_name, row_id, operation) VALUES ('x', 'x', 'x', 'site', 1, 'INSERT')",
        "TRUNCATE change",
        "UPDATE file SET name = 'x'",
        "DELETE FROM linework",
    ] {
        let error = sqlx::query(sql)
            .execute(&mut qgis)
            .await
            .err()
            .unwrap_or_else(|| panic!("{sql} was allowed"));
        let code = error.as_database_error().and_then(|e| e.code()).map(|c| c.into_owned());
        assert_eq!(code.as_deref(), Some("42501"), "{sql}: {error}");
    }
}

#[tokio::test]
async fn read_only_role_reads_but_cannot_write() {
    let db = TestDb::new().await;
    let pool = db.migrated_pool().await;
    seed(&pool).await;
    let mut ro = PgConnection::connect_with(&db.options("house_ro", RO_PASSWORD))
        .await
        .expect("connect as house_ro");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM change")
        .fetch_one(&mut ro)
        .await
        .expect("may read");
    assert!(count > 0);
    let error = sqlx::query("UPDATE site SET name = 'x'")
        .execute(&mut ro)
        .await
        .expect_err("refused");
    let code = error.as_database_error().and_then(|e| e.code()).map(|c| c.into_owned());
    assert_eq!(code.as_deref(), Some("42501"));
}

/// The site's origin is UTM 32N (500000, 0): the zone's central meridian, 9
/// degrees east, on the equator. A point 0.001 degrees east and north of it
/// lies k0 a (0.001 in radians) east, and k0 a (1 - e^2) (0.001 in radians)
/// north, where k0 = 0.9996 and a, e are the WGS 84 ellipsoid's; the terms
/// dropped are below a micrometre. The site's y axis is turned 30 degrees
/// clockwise from grid north.
#[tokio::test]
async fn wgs84_point_lands_at_the_expected_local_coordinates_and_back() {
    let db = TestDb::new().await;
    let pool = db.migrated_pool().await;
    let site: i64 = sqlx::query_scalar(
        "INSERT INTO site (name, origin, rotation) VALUES ('frame', ST_SetSRID(ST_MakePoint(500000, 0), 32632), 30) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("site");
    let (k0, a, e2) = (0.9996, 6_378_137.0, 0.006_694_379_990_14);
    let step = 0.001_f64.to_radians();
    let (east, north) = (k0 * a * step, k0 * a * (1.0 - e2) * step);
    let (sin, cos) = 30_f64.to_radians().sin_cos();
    let expected = (east * cos - north * sin, east * sin + north * cos);

    let (x, y, lon, lat): (f64, f64, f64, f64) = sqlx::query_as(
        "WITH l AS (SELECT house_to_local(ST_SetSRID(ST_MakePoint(9.001, 0.001), 4326), $1) AS g),
              w AS (SELECT house_from_local(g, $1, 4326) AS g FROM l)
         SELECT ST_X(l.g), ST_Y(l.g), ST_X(w.g), ST_Y(w.g) FROM l, w",
    )
    .bind(site)
    .fetch_one(&pool)
    .await
    .expect("frame functions");
    assert!(
        (x - expected.0).abs() < 0.01 && (y - expected.1).abs() < 0.01,
        "({x}, {y}) vs {expected:?}"
    );
    // 1 cm is about 9e-8 degrees here.
    assert!(
        (lon - 9.001).abs() < 9e-8 && (lat - 0.001).abs() < 9e-8,
        "({lon}, {lat})"
    );
}

#[tokio::test]
async fn frame_functions_refuse_a_site_without_a_georeference() {
    let db = TestDb::new().await;
    let pool = db.migrated_pool().await;
    let site: i64 = sqlx::query_scalar("INSERT INTO site (name) VALUES ('nowhere') RETURNING id")
        .fetch_one(&pool)
        .await
        .expect("site");
    let error = sqlx::query("SELECT house_to_local(ST_SetSRID(ST_MakePoint(9, 45), 4326), $1)")
        .bind(site)
        .execute(&pool)
        .await
        .expect_err("refused");
    assert!(error.to_string().contains("has no georeference"), "{error}");
}

#[tokio::test]
async fn linework_refuses_empty_geometry() {
    let db = TestDb::new().await;
    let pool = db.migrated_pool().await;
    let (site, _, _) = seed(&pool).await;
    let import: i64 = sqlx::query_scalar(
        "WITH f AS (INSERT INTO file (name, media_type, size, sha256) VALUES ('a.dxf', 'image/vnd.dxf', 1, repeat('0', 64)) RETURNING id),
              fl AS (SELECT id FROM floor WHERE site_id = $1)
         INSERT INTO dxf_import (file_id, sha256, floor_id, unit, options, summary)
         SELECT f.id, repeat('0', 64), fl.id, 'm', '{}', '{}' FROM f, fl RETURNING id",
    )
    .bind(site)
    .fetch_one(&pool)
    .await
    .expect("import");
    let error = sqlx::query(
        "INSERT INTO linework (dxf_import_id, floor_id, layer, entity, geom)
         SELECT $1, floor_id, '0', 'LINE', 'LINESTRING EMPTY' FROM dxf_import WHERE id = $1",
    )
    .bind(import)
    .execute(&pool)
    .await
    .expect_err("refused");
    let code = error.as_database_error().and_then(|e| e.code()).map(|c| c.into_owned());
    assert_eq!(code.as_deref(), Some("23514"), "{error}");
}

#[tokio::test]
async fn linework_and_imports_belong_to_a_floor_or_a_site() {
    let db = TestDb::new().await;
    let pool = db.migrated_pool().await;
    let (site, _, _) = seed(&pool).await;
    let file: i64 = sqlx::query_scalar(
        "INSERT INTO file (name, media_type, size, sha256) VALUES ('a.dxf', 'image/vnd.dxf', 1, repeat('0', 64)) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("file");
    let import = |site: Option<i64>| {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO dxf_import (file_id, sha256, site_id, unit, options, summary)
             VALUES ($1, repeat('0', 64), $2, 'm', '{}', '{}') RETURNING id",
        )
        .bind(file)
        .bind(site)
        .fetch_one(&pool)
    };
    let nowhere = import(None).await.expect_err("an import must have a place");
    assert_eq!(
        nowhere.as_database_error().and_then(|e| e.constraint()),
        Some("dxf_import_has_a_place")
    );
    let grounds = import(Some(site)).await.expect("an import onto the site");
    let twice = import(Some(site))
        .await
        .expect_err("one live import of a drawing per site");
    assert_eq!(
        twice.as_database_error().and_then(|e| e.constraint()),
        Some("dxf_import_site_live")
    );
    let line = |site: Option<i64>| {
        sqlx::query(
            "INSERT INTO linework (dxf_import_id, site_id, layer, entity, geom)
             VALUES ($1, $2, '0', 'LINE', 'LINESTRING(0 0, 1 1)')",
        )
        .bind(grounds)
        .bind(site)
        .execute(&pool)
    };
    let error = line(None).await.expect_err("linework must have a place");
    assert_eq!(
        error.as_database_error().and_then(|e| e.constraint()),
        Some("linework_has_a_place")
    );
    line(Some(site)).await.expect("linework on the site");
    // Sites and floors start with no attributes, and only an object will do.
    let attributes: Value = sqlx::query_scalar("SELECT attributes FROM site WHERE id = $1")
        .bind(site)
        .fetch_one(&pool)
        .await
        .expect("attributes");
    assert_eq!(attributes, serde_json::json!({}));
    let error = sqlx::query("UPDATE floor SET attributes = '[]' WHERE site_id = $1")
        .bind(site)
        .execute(&pool)
        .await
        .expect_err("not an object");
    let code = error.as_database_error().and_then(|e| e.code()).map(|c| c.into_owned());
    assert_eq!(code.as_deref(), Some("23514"), "{error}");
}

#[tokio::test]
async fn the_placement_migration_fills_from_the_position_attribute() {
    let db = TestDb::new().await;
    let pool = house_mcp::db::connect(&db.house_url()).await.expect("connect as house");
    // The schema as it was before placement, seeded the way objects were.
    house_mcp::db::MIGRATOR.run_to(3, &pool).await.expect("migrations to 3");
    let mut tx = house_mcp::db::begin_write(&pool, "jesse", "seeding the test")
        .await
        .expect("begin");
    let site: i64 = sqlx::query_scalar("INSERT INTO site (name) VALUES ('test') RETURNING id")
        .fetch_one(&mut *tx)
        .await
        .expect("site");
    let mut ids = Vec::new();
    for (name, attributes) in [
        ("stacked", r#"{"position": "site_unplaced", "note": "kept"}"#),
        ("in its room", r#"{"position": "room_centre"}"#),
        ("by eye", r#"{"position": "paced out"}"#),
        ("surveyed", r#"{}"#),
    ] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO object (site_id, kind, name, geom, attributes) VALUES ($1, 'camera', $2, ST_MakePoint(7, -3), $3::jsonb) RETURNING id",
        )
        .bind(site)
        .bind(name)
        .bind(attributes)
        .fetch_one(&mut *tx)
        .await
        .expect("object");
        ids.push(id);
    }
    tx.commit().await.expect("commit");

    house_mcp::db::MIGRATOR
        .run(&pool)
        .await
        .expect("the rest of the migrations");
    let rows: Vec<(i64, String, Value)> = sqlx::query_as("SELECT id, placement, attributes FROM object ORDER BY id")
        .fetch_all(&pool)
        .await
        .expect("objects");
    let placements: Vec<(i64, &str)> = rows.iter().map(|(id, p, _)| (*id, p.as_str())).collect();
    assert_eq!(
        placements,
        [
            (ids[0], "unplaced"),
            (ids[1], "approximate"),
            (ids[2], "approximate"),
            (ids[3], "exact")
        ]
    );
    // The attribute stays as it was.
    assert_eq!(
        rows[0].2,
        serde_json::json!({"position": "site_unplaced", "note": "kept"})
    );
    // The fill is recorded like any other write.
    let fill = changes(&pool, "object", ids[0]).await;
    let last = fill.last().expect("a change");
    assert_eq!(
        (last.author.as_str(), last.reason.as_str(), last.operation.as_str()),
        ("migration 0004", "placement from the position attribute", "UPDATE")
    );
    let surveyed = changes(&pool, "object", ids[3]).await;
    assert_eq!(surveyed.len(), 1, "an object with no position is not written");
    // And the placement is refused anything but the three values.
    let refused = sqlx::query("UPDATE object SET placement = 'guessed' WHERE id = $1")
        .bind(ids[3])
        .execute(&pool)
        .await
        .expect_err("refused");
    assert!(refused.to_string().contains("object_placement"), "{refused}");
}

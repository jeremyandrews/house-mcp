//! What the database and end to end tests share: a fresh database per test,
//! set up the way production is, and a synthetic drawing.
//!
//! `HOUSE_TEST_DATABASE_URL` must name a PostGIS server as a superuser, such
//! as `postgres://postgres@localhost:55432/postgres`.

#![allow(dead_code)]

use std::sync::atomic::{AtomicU32, Ordering};

use dxf::{
    Block, Drawing, Point,
    entities::{Circle, Entity, EntityType, Insert, LwPolyline, MText, Text},
    enums::{AcadVersion, Units},
};
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool, postgres::PgConnectOptions};

pub const HOUSE_PASSWORD: &str = "house-test-password";
pub const QGIS_PASSWORD: &str = "qgis-test-password";
pub const RO_PASSWORD: &str = "ro-test-password";

pub struct TestDb {
    pub name: String,
    admin: PgConnectOptions,
}

impl TestDb {
    /// A new database owned by `house`, with PostGIS, and the three roles
    /// production has. Role creation tolerates another test having just made
    /// the same role.
    pub async fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let url =
            std::env::var("HOUSE_TEST_DATABASE_URL").expect("HOUSE_TEST_DATABASE_URL names a PostGIS superuser URL");
        let admin: PgConnectOptions = url.parse().expect("a database URL");
        let name = format!(
            "house_test_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut conn = PgConnection::connect_with(&admin).await.expect("admin connection");
        for (role, password) in [
            ("house", HOUSE_PASSWORD),
            ("house_qgis", QGIS_PASSWORD),
            ("house_ro", RO_PASSWORD),
        ] {
            let sql = format!("CREATE ROLE {role} LOGIN PASSWORD '{password}'");
            if let Err(e) = conn.execute(sqlx::raw_sql(AssertSqlSafe(sql))).await {
                let duplicate = e
                    .as_database_error()
                    .and_then(|d| d.code())
                    .is_some_and(|c| c == "42710" || c == "23505");
                assert!(duplicate, "creating {role}: {e}");
            }
        }
        conn.execute(sqlx::raw_sql(AssertSqlSafe(format!(
            "CREATE DATABASE {name} OWNER house"
        ))))
        .await
        .expect("create database");
        let mut db = PgConnection::connect_with(&admin.clone().database(&name))
            .await
            .expect("connect to test database");
        db.execute("CREATE EXTENSION postgis").await.expect("postgis");
        Self { name, admin }
    }

    /// A connection as `house_qgis`, named the way QGIS names its own.
    pub async fn qgis(&self) -> PgConnection {
        PgConnection::connect_with(&self.options("house_qgis", QGIS_PASSWORD).application_name("QGIS 3.40"))
            .await
            .expect("connect as house_qgis")
    }

    pub fn options(&self, role: &str, password: &str) -> PgConnectOptions {
        self.admin
            .clone()
            .database(&self.name)
            .username(role)
            .password(password)
    }

    pub fn house_url(&self) -> String {
        let host = self.admin.get_host();
        let port = self.admin.get_port();
        format!("postgres://house:{HOUSE_PASSWORD}@{host}:{port}/{}", self.name)
    }

    /// A pool as the server's role, with the schema migrated.
    pub async fn migrated_pool(&self) -> PgPool {
        let pool = house_mcp::db::connect(&self.house_url())
            .await
            .expect("connect as house");
        house_mcp::db::MIGRATOR.run(&pool).await.expect("migrations");
        pool
    }
}

/// Two rooms side by side in millimetres, labelled by a TEXT and an MTEXT,
/// with a ceiling light block inserted in the kitchen:
///
/// ```text
///   y=3000 +-----------+----------------+
///          |  Kitchen  |  Living room   |
///          |     *     |                |
///   y=0    +-----------+----------------+
///          x=0      x=4000           x=9000
/// ```
///
/// The light is at (2000, 2500) mm.
pub fn two_room_dxf() -> Vec<u8> {
    let mut drawing = drawing(Units::Millimeters);
    drawing.add_entity(on_layer("A-ROOM", rectangle(0.0, 0.0, 4000.0, 3000.0)));
    drawing.add_entity(on_layer("A-ROOM", rectangle(4000.0, 0.0, 9000.0, 3000.0)));
    drawing.add_entity(on_layer(
        "A-TEXT",
        EntityType::Text(Text {
            location: Point::new(1500.0, 1200.0, 0.0),
            text_height: 250.0,
            value: "Kitchen".into(),
            ..Default::default()
        }),
    ));
    drawing.add_entity(on_layer(
        "A-TEXT",
        EntityType::MText(MText {
            insertion_point: Point::new(6000.0, 1500.0, 0.0),
            initial_text_height: 250.0,
            text: r"{\fArial|b0;Living}\Proom".into(),
            ..Default::default()
        }),
    ));
    let mut light = Block {
        name: "LIGHT".into(),
        base_point: Point::origin(),
        ..Default::default()
    };
    light
        .entities
        .push(Entity::new(EntityType::Circle(Circle::new(Point::origin(), 150.0))));
    drawing.add_block(light);
    drawing.add_entity(on_layer(
        "E-LIGHT",
        EntityType::Insert(Insert {
            name: "LIGHT".into(),
            location: Point::new(2000.0, 2500.0, 0.0),
            ..Default::default()
        }),
    ));
    save(&drawing)
}

/// A drawing in a DXF version new enough to carry $INSUNITS and LWPOLYLINE.
pub fn drawing(units: Units) -> Drawing {
    let mut drawing = Drawing::new();
    drawing.header.version = AcadVersion::R2018;
    drawing.header.default_drawing_units = units;
    drawing
}

pub fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> EntityType {
    let mut poly = LwPolyline::default();
    for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
        poly.vertices.push(dxf::LwPolylineVertex {
            x,
            y,
            ..Default::default()
        });
    }
    poly.set_is_closed(true);
    EntityType::LwPolyline(poly)
}

pub fn on_layer(layer: &str, specific: EntityType) -> Entity {
    let mut entity = Entity::new(specific);
    entity.common.layer = layer.into();
    entity
}

pub fn save(drawing: &Drawing) -> Vec<u8> {
    let mut bytes = Vec::new();
    drawing.save(&mut bytes).expect("write DXF");
    bytes
}

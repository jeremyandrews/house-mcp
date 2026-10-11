use std::collections::BTreeMap;

use axum::http::request::Parts;
use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData,
    handler::server::{tool::Extension, wrapper::Parameters},
    model::CallToolResult,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use super::{House, Kind, Links, json_result, reason, wkb, writer};
use crate::{MIB, db, error, files::FileStore, geom::MAX_VERTICES};

/// The largest file a tool call may carry. Larger files go through
/// `POST /api/files`.
const MAX_TOOL_FILE: usize = 25 * MIB;
/// The geometry an object may have. The same list is a CHECK on `object.geom`.
pub(super) const OBJECT_GEOMETRY_TYPES: &[&str] = &[
    "POINT",
    "LINESTRING",
    "POLYGON",
    "MULTIPOINT",
    "MULTILINESTRING",
    "MULTIPOLYGON",
];
const SPACE_KINDS: [&str; 6] = ["room", "terrace", "garden_bed", "pool", "parcel", "building"];

/// The same statement for whichever table a [`Kind`] names. Every variant is
/// a literal, so no SQL is assembled at run time.
macro_rules! per_table {
    ($kind:expr, $a:literal, $b:literal) => {
        match $kind {
            Kind::Site => concat!($a, "site", $b),
            Kind::Floor => concat!($a, "floor", $b),
            Kind::Space => concat!($a, "space", $b),
            Kind::Object => concat!($a, "object", $b),
            Kind::File => concat!($a, "file", $b),
            Kind::Note => concat!($a, "note", $b),
        }
    };
    ($kind:expr, $a:literal, $b:literal, $c:literal) => {
        match $kind {
            Kind::Site => concat!($a, "site", $b, "site", $c),
            Kind::Floor => concat!($a, "floor", $b, "floor", $c),
            Kind::Space => concat!($a, "space", $b, "space", $c),
            Kind::Object => concat!($a, "object", $b, "object", $c),
            Kind::File => concat!($a, "file", $b, "file", $c),
            Kind::Note => concat!($a, "note", $b, "note", $c),
        }
    };
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Origin {
    /// EPSG code of the coordinates, such as 4326 for longitude and latitude.
    pub epsg: i32,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateSiteParams {
    pub name: String,
    /// Where the site frame's origin is on the earth. Needed before anything
    /// can be imported from a geographic coordinate system.
    pub origin: Option<Origin>,
    /// Degrees clockwise from grid north to the site's y axis. Default 0.
    pub rotation: Option<f64>,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetSiteOriginParams {
    pub site_id: i64,
    /// Where the site frame's origin is on the earth, in any EPSG coordinate
    /// system: `{"epsg": 4326, "x": longitude, "y": latitude}` for WGS 84.
    pub origin: Origin,
    /// Degrees clockwise from grid north to the site's y axis. Omit to keep
    /// the stored rotation.
    pub rotation: Option<f64>,
    pub reason: String,
}

/// A field a caller may leave out, send as null, or send with a value:
/// `None` when left out, `Some(None)` for null.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpsertFloorParams {
    /// Omit to create a floor; give to replace an existing floor's fields.
    pub id: Option<i64>,
    pub site_id: i64,
    pub name: String,
    /// 0 for the ground floor, negative below it.
    pub level: i32,
    /// Metres above the site's datum. Default 0.
    pub elevation: Option<f64>,
    pub ceiling_height: Option<f64>,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpsertSpaceParams {
    /// Omit to create a space; give to update one, when every field left
    /// out keeps its stored value.
    pub id: Option<i64>,
    pub site_id: i64,
    /// Null or left out on create for an outdoor area. On update, left out
    /// keeps the floor and null moves the space outdoors.
    #[serde(default, deserialize_with = "present")]
    pub floor_id: Option<Option<i64>>,
    /// Required on create.
    pub name: Option<String>,
    /// room, terrace, garden_bed, pool, parcel or building. Required on
    /// create.
    pub kind: Option<String>,
    /// A GeoJSON Polygon in the site frame, metres. Required on create.
    pub geometry: Option<Value>,
    /// On update, left out keeps it and null clears it.
    #[serde(default, deserialize_with = "present")]
    pub wall_material: Option<Option<String>>,
    /// Replaces the attributes when given.
    pub attributes: Option<serde_json::Map<String, Value>>,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpsertObjectParams {
    /// Omit to create an object; give to update one, when every field left
    /// out keeps its stored value.
    pub id: Option<i64>,
    pub site_id: i64,
    /// Null or left out on create for something outdoors. On update, left
    /// out keeps the floor and null moves the object outdoors.
    #[serde(default, deserialize_with = "present")]
    pub floor_id: Option<Option<i64>>,
    /// light, switch, socket, access_point, camera, sensor, speaker, tree,
    /// pipe, cable_run, opening, or another lowercase word. Required on
    /// create.
    pub kind: Option<String>,
    /// Required on create.
    pub name: Option<String>,
    /// GeoJSON Point, LineString or Polygon (or a Multi of one) in the site
    /// frame, metres. Required on create.
    pub geometry: Option<Value>,
    /// Metres above the floor, or above the ground outdoors. On update, left
    /// out keeps it and null clears it.
    #[serde(default, deserialize_with = "present")]
    pub height: Option<Option<f64>>,
    /// Replaces the outside identifiers when given; see link_ref to change one.
    pub refs: Option<BTreeMap<String, String>>,
    /// Replaces the attributes when given; see set_attributes to change some.
    pub attributes: Option<serde_json::Map<String, Value>>,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttributeKind {
    Site,
    Floor,
    Space,
    Object,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetAttributesParams {
    pub kind: AttributeKind,
    pub id: i64,
    /// Keys to add or overwrite.
    #[serde(default)]
    pub set: serde_json::Map<String, Value>,
    /// Keys to remove.
    #[serde(default)]
    pub remove: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LinkRefParams {
    pub object_id: i64,
    /// Such as `ha_entity_id` or `unifi_mac`.
    pub key: String,
    /// The identifier; omit to remove the key.
    pub value: Option<String>,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddNoteParams {
    /// Markdown.
    pub body: String,
    /// Where the knowledge came from, such as "walked the garden, 3 October".
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub links: Links,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UploadFileParams {
    pub name: String,
    /// Guessed from the name when omitted.
    pub media_type: Option<String>,
    /// Text content. Give this or `base64`.
    pub text: Option<String>,
    /// Binary content, base64, at most 25 MiB decoded.
    pub base64: Option<String>,
    #[serde(default)]
    pub links: Links,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThingParams {
    pub kind: Kind,
    pub id: i64,
    pub reason: String,
}

#[tool_router(router = write_router, vis = "pub(super)")]
impl House {
    #[tool(
        description = "Create a site, optionally with its origin on the earth (any EPSG code) and its rotation from grid north."
    )]
    async fn create_site(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<CreateSiteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        let rotation = p.rotation.unwrap_or(0.0);
        if !(rotation > -360.0 && rotation < 360.0) {
            return Err(error::invalid("rotation", "must be between -360 and 360 degrees"));
        }
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        if let Some(origin) = &p.origin {
            check_epsg(&mut tx, "origin.epsg", origin.epsg).await?;
        }
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO site (name, origin, rotation)
             VALUES ($1, CASE WHEN $2::int IS NULL THEN NULL
                              ELSE ST_Transform(ST_SetSRID(ST_MakePoint($3, $4), $2), 32632) END, $5)
             RETURNING id",
        )
        .bind(p.name.trim())
        .bind(p.origin.as_ref().map(|o| o.epsg))
        .bind(p.origin.as_ref().map(|o| o.x))
        .bind(p.origin.as_ref().map(|o| o.y))
        .bind(rotation)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| origin_error(e, p.origin.as_ref()))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": id }))
    }

    #[tool(
        description = "Set or replace where a site's frame is on the earth: its origin, in any EPSG coordinate system, and optionally its rotation, degrees clockwise from grid north to the site's y axis (left out, the stored rotation stays). Stored geometry is in the site frame and does not move; what changes is where that frame lies, which import_geojson and any other conversion use from then on. Recorded in history with the reason, like every write. Returns the origin in WGS 84 and the rotation. Example: {\"site_id\": 3, \"origin\": {\"epsg\": 4326, \"x\": 9.19, \"y\": 45.46}, \"rotation\": 12.5, \"reason\": \"origin at the south west corner of the survey\"}."
    )]
    async fn set_site_origin(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SetSiteOriginParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        if p.rotation.is_some_and(|r| !(r > -360.0 && r < 360.0)) {
            return Err(error::invalid("rotation", "must be between -360 and 360 degrees"));
        }
        if !(p.origin.x.is_finite() && p.origin.y.is_finite()) {
            return Err(error::invalid("origin", "x and y must be numbers"));
        }
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        check_epsg(&mut tx, "origin.epsg", p.origin.epsg).await?;
        let row: Option<(f64, f64, f64)> = sqlx::query_as(
            "UPDATE site SET origin = ST_Transform(ST_SetSRID(ST_MakePoint($2, $3), $4), 32632), rotation = coalesce($5, rotation)
             WHERE id = $1 AND deleted_at IS NULL
             RETURNING ST_X(ST_Transform(origin, 4326)), ST_Y(ST_Transform(origin, 4326)), rotation",
        )
        .bind(p.site_id)
        .bind(p.origin.x)
        .bind(p.origin.y)
        .bind(p.origin.epsg)
        .bind(p.rotation)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| origin_error(e, Some(&p.origin)))?;
        let (lon, lat, rotation) = row.ok_or_else(|| error::invalid("site_id", "no live site with this id"))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": p.site_id, "origin_wgs84": { "lon": lon, "lat": lat }, "rotation": rotation }))
    }

    #[tool(description = "Create a floor, or replace an existing floor's fields when `id` is given.")]
    async fn upsert_floor(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpsertFloorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        if p.ceiling_height.is_some_and(|h| h <= 0.0) {
            return Err(error::invalid("ceiling_height", "must be positive"));
        }
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        let id: Option<i64> = sqlx::query_scalar(
            "WITH created AS (
                 INSERT INTO floor (site_id, name, level, elevation, ceiling_height)
                 SELECT $2, $3, $4, $5, $6 WHERE $1::bigint IS NULL RETURNING id
             ), updated AS (
                 UPDATE floor SET site_id = $2, name = $3, level = $4, elevation = $5, ceiling_height = $6
                 WHERE id = $1 AND deleted_at IS NULL RETURNING id
             )
             SELECT id FROM created UNION ALL SELECT id FROM updated",
        )
        .bind(p.id)
        .bind(p.site_id)
        .bind(p.name.trim())
        .bind(p.level)
        .bind(p.elevation.unwrap_or(0.0))
        .bind(p.ceiling_height)
        .fetch_optional(&mut *tx)
        .await
        .map_err(error::db)?;
        let id = id.ok_or_else(|| error::invalid("id", "no live floor with this id"))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": id }))
    }

    #[tool(
        description = "Create a space (a room, terrace, garden bed, pool, parcel or building outline), or update one when `id` is given. On create, `name`, `kind` and `geometry` are required. On update every field but `id`, `site_id` and `reason` is optional, and a field left out keeps its stored value, so a rename is {\"id\": 7, \"site_id\": 1, \"name\": \"Pantry\", \"reason\": \"...\"}; send `floor_id` or `wall_material` as null to clear it. `attributes`, when given, replaces them all; set_attributes changes some."
    )]
    async fn upsert_space(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpsertSpaceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        if p.id.is_none() {
            required_on_create(
                "space",
                [
                    ("name", p.name.is_some()),
                    ("kind", p.kind.is_some()),
                    ("geometry", p.geometry.is_some()),
                ],
            )?;
        }
        if let Some(kind) = &p.kind {
            check_space_kind("kind", kind)?;
        }
        let geometry = p.geometry.as_ref().map(|g| wkb("geometry", g)).transpose()?;
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        if let Some(geometry) = &geometry {
            check_geometry(&mut tx, "geometry", geometry, &["POLYGON"]).await?;
        }
        let id: Option<i64> = sqlx::query_scalar(
            "WITH created AS (
                 INSERT INTO space (site_id, floor_id, name, kind, geom, wall_material, attributes)
                 SELECT $2, $3, $4, $5, ST_GeomFromWKB($6, 0), $7, coalesce($8, '{}')
                 WHERE $1::bigint IS NULL RETURNING id
             ), updated AS (
                 UPDATE space SET site_id = $2, floor_id = CASE WHEN $9 THEN $3 ELSE floor_id END,
                                  name = coalesce($4, name), kind = coalesce($5, kind),
                                  geom = coalesce(ST_GeomFromWKB($6, 0), geom),
                                  wall_material = CASE WHEN $10 THEN $7 ELSE wall_material END,
                                  attributes = coalesce($8, attributes)
                 WHERE id = $1 AND deleted_at IS NULL RETURNING id
             )
             SELECT id FROM created UNION ALL SELECT id FROM updated",
        )
        .bind(p.id)
        .bind(p.site_id)
        .bind(p.floor_id.flatten())
        .bind(p.name.as_deref().map(str::trim))
        .bind(&p.kind)
        .bind(&geometry)
        .bind(p.wall_material.clone().flatten())
        .bind(p.attributes.map(Value::Object))
        .bind(p.floor_id.is_some())
        .bind(p.wall_material.is_some())
        .fetch_optional(&mut *tx)
        .await
        .map_err(error::db)?;
        let id = id.ok_or_else(|| error::invalid("id", "no live space with this id"))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": id }))
    }

    #[tool(
        description = "Create an object (a light, switch, socket, access point, camera, sensor, speaker, tree, pipe, cable run, opening or anything else), or update one when `id` is given. On create, `kind`, `name` and `geometry` are required. On update every field but `id`, `site_id` and `reason` is optional, and a field left out keeps its stored value, so a rename is {\"id\": 41, \"site_id\": 1, \"name\": \"Reading lamp\", \"reason\": \"...\"}; send `floor_id` or `height` as null to clear it. `refs` and `attributes`, when given, replace them all; link_ref and set_attributes change some.\n\nAn `opening` is a door, arch or open doorway that render_plan draws as a gap in the wall with a jamb tick at each end: its geometry is a LineString of exactly two points along the wall's centre line, from one jamb to the other, on the opening's floor. Attributes: `opening_type` is `door`, `arch` (two dashed lines across the gap, one along each face of the wall) or `opening` (the gap alone), default `opening`; `wall_depth` is the wall's thickness in metres, a positive number, default 0.6, and the gap cut is that wide across the wall. A door also takes `hinge`, the `start` or `end` of the line the leaf hangs from (default `start`), `swing`, the `left` or `right` of the line's direction it opens towards (default `left`), and `leaves`, 1 or 2 (default 1; 2 hangs a half leaf on each jamb); it is drawn as its leaf square to the wall and the quarter circle of its swing. `connects`, a list of exactly two space ids, says which spaces the opening joins; without it the connections tool finds them from the geometry. Any other geometry is drawn as an ordinary object. Door example: {\"site_id\": 1, \"floor_id\": 2, \"kind\": \"opening\", \"name\": \"Door to the hall\", \"geometry\": {\"type\": \"LineString\", \"coordinates\": [[4.0, 1.2], [4.0, 2.1]]}, \"attributes\": {\"opening_type\": \"door\", \"hinge\": \"end\", \"swing\": \"right\", \"wall_depth\": 0.3, \"connects\": [12, 13]}, \"reason\": \"door on the survey\"}. Arch example: {\"site_id\": 1, \"floor_id\": 2, \"kind\": \"opening\", \"name\": \"Arch to the kitchen\", \"geometry\": {\"type\": \"LineString\", \"coordinates\": [[7.0, 0.5], [7.0, 2.1]]}, \"attributes\": {\"opening_type\": \"arch\", \"wall_depth\": 0.45}, \"reason\": \"arch on the survey\"}."
    )]
    async fn upsert_object(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpsertObjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        if p.id.is_none() {
            required_on_create(
                "object",
                [
                    ("kind", p.kind.is_some()),
                    ("name", p.name.is_some()),
                    ("geometry", p.geometry.is_some()),
                ],
            )?;
        }
        let kind = p.kind.as_deref().map(|kind| object_kind("kind", kind)).transpose()?;
        if let Some(refs) = &p.refs {
            for key in refs.keys() {
                check_key("refs", key)?;
            }
        }
        let geometry = p.geometry.as_ref().map(|g| wkb("geometry", g)).transpose()?;
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        if let Some(geometry) = &geometry {
            check_geometry(&mut tx, "geometry", geometry, OBJECT_GEOMETRY_TYPES).await?;
        }
        let refs = p.refs.map(|refs| json!(refs));
        let id: Option<i64> = sqlx::query_scalar(
            "WITH created AS (
                 INSERT INTO object (site_id, floor_id, kind, name, geom, height, refs, attributes)
                 SELECT $2, $3, $4, $5, ST_GeomFromWKB($6, 0), $7, coalesce($8, '{}'), coalesce($9, '{}')
                 WHERE $1::bigint IS NULL RETURNING id
             ), updated AS (
                 UPDATE object SET site_id = $2, floor_id = CASE WHEN $10 THEN $3 ELSE floor_id END,
                                   kind = coalesce($4, kind), name = coalesce($5, name),
                                   geom = coalesce(ST_GeomFromWKB($6, 0), geom),
                                   height = CASE WHEN $11 THEN $7 ELSE height END,
                                   refs = coalesce($8, refs), attributes = coalesce($9, attributes)
                 WHERE id = $1 AND deleted_at IS NULL RETURNING id
             )
             SELECT id FROM created UNION ALL SELECT id FROM updated",
        )
        .bind(p.id)
        .bind(p.site_id)
        .bind(p.floor_id.flatten())
        .bind(&kind)
        .bind(p.name.as_deref().map(str::trim))
        .bind(&geometry)
        .bind(p.height.flatten())
        .bind(refs)
        .bind(p.attributes.map(Value::Object))
        .bind(p.floor_id.is_some())
        .bind(p.height.is_some())
        .fetch_optional(&mut *tx)
        .await
        .map_err(error::db)?;
        let id = id.ok_or_else(|| error::invalid("id", "no live object with this id"))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": id }))
    }

    #[tool(
        description = "Add, change or remove some attributes of a site, floor, space or object (`kind`: site, floor, space or object), leaving the rest as they are. Site and floor attributes suit facts about the whole, such as which drawing a floor came from; list_sites and get_floor return them. `common_name` is the family's nickname for a space or object and is what render_plan labels it with, ahead of `name_en` and the stored `name`. Example: {\"kind\": \"floor\", \"id\": 2, \"set\": {\"source_drawing\": \"plan sheet 2\"}, \"reason\": \"where the floor came from\"}."
    )]
    async fn set_attributes(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SetAttributesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        if p.set.is_empty() && p.remove.is_empty() {
            return Err(error::invalid("set", "nothing to set or remove"));
        }
        let sql = match p.kind {
            AttributeKind::Site => {
                "UPDATE site SET attributes = (attributes || $2) - $3::text[] WHERE id = $1 AND deleted_at IS NULL RETURNING attributes"
            }
            AttributeKind::Floor => {
                "UPDATE floor SET attributes = (attributes || $2) - $3::text[] WHERE id = $1 AND deleted_at IS NULL RETURNING attributes"
            }
            AttributeKind::Space => {
                "UPDATE space SET attributes = (attributes || $2) - $3::text[] WHERE id = $1 AND deleted_at IS NULL RETURNING attributes"
            }
            AttributeKind::Object => {
                "UPDATE object SET attributes = (attributes || $2) - $3::text[] WHERE id = $1 AND deleted_at IS NULL RETURNING attributes"
            }
        };
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        let attributes: Option<Value> = sqlx::query_scalar(sql)
            .bind(p.id)
            .bind(Value::Object(p.set))
            .bind(&p.remove)
            .fetch_optional(&mut *tx)
            .await
            .map_err(error::db)?;
        let attributes = attributes.ok_or_else(|| error::invalid("id", "no live row of this kind with this id"))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": p.id, "attributes": attributes }))
    }

    #[tool(
        description = "Set or remove one outside identifier on an object, such as its Home Assistant entity id or UniFi MAC address."
    )]
    async fn link_ref(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<LinkRefParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        check_key("key", &p.key)?;
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        let refs: Option<Value> = sqlx::query_scalar(
            "UPDATE object SET refs = CASE WHEN $3::text IS NULL THEN refs - $2
                                           ELSE refs || jsonb_build_object($2, $3) END
             WHERE id = $1 AND deleted_at IS NULL RETURNING refs",
        )
        .bind(p.object_id)
        .bind(&p.key)
        .bind(&p.value)
        .fetch_optional(&mut *tx)
        .await
        .map_err(error::db)?;
        let refs = refs.ok_or_else(|| error::invalid("object_id", "no live object with this id"))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": p.object_id, "refs": refs }))
    }

    #[tool(
        description = "Add a markdown note, with where it came from, optionally linked to a site, floor, space or object."
    )]
    async fn add_note(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO note (body, source, site_id, floor_id, space_id, object_id)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(&p.body)
        .bind(&p.source)
        .bind(p.links.site_id)
        .bind(p.links.floor_id)
        .bind(p.links.space_id)
        .bind(p.links.object_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(error::db)?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": id }))
    }

    #[tool(
        description = "Store a file (text inline, or binary as base64 up to 25 MiB), optionally linked to a site, floor, space or object. Larger files go through POST /api/files."
    )]
    async fn upload_file(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UploadFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let bytes = match (p.text, p.base64) {
            (Some(text), None) => {
                if text.len() > MAX_TOOL_FILE {
                    return Err(error::invalid(
                        "text",
                        format!("is larger than {} MiB", MAX_TOOL_FILE / MIB),
                    ));
                }
                text.into_bytes()
            }
            (None, Some(encoded)) => {
                if encoded.len() > MAX_TOOL_FILE.div_ceil(3) * 4 + 4 {
                    return Err(error::invalid(
                        "base64",
                        format!("decodes to more than {} MiB", MAX_TOOL_FILE / MIB),
                    ));
                }
                let bytes = STANDARD
                    .decode(encoded.trim())
                    .map_err(|e| error::invalid("base64", format!("not base64: {e}")))?;
                if bytes.len() > MAX_TOOL_FILE {
                    return Err(error::invalid(
                        "base64",
                        format!("decodes to more than {} MiB", MAX_TOOL_FILE / MIB),
                    ));
                }
                bytes
            }
            _ => return Err(error::invalid("text", "give exactly one of text and base64")),
        };
        let stored = store_file(
            &self.db,
            &self.files,
            author,
            &p.reason,
            NewFile {
                name: &p.name,
                media_type: p.media_type.as_deref(),
                bytes: &bytes,
                links: &p.links,
            },
        )
        .await?;
        json_result(stored)
    }

    #[tool(
        description = "Mark a site, floor, space, object, file or note as deleted. Nothing is removed; restore brings it back."
    )]
    async fn delete(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ThingParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        let deleted: Option<i64> = sqlx::query_scalar(per_table!(
            p.kind,
            "UPDATE ",
            " SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL RETURNING id"
        ))
        .bind(p.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(error::db)?;
        deleted.ok_or_else(|| error::invalid("id", "no live row of this kind with this id"))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "deleted": { "kind": p.kind, "id": p.id } }))
    }

    #[tool(
        description = "Bring back a deleted site, floor, space, object, file or note, including one removed outright by another client, from its recorded history."
    )]
    async fn restore(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ThingParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        let undeleted: Option<i64> = sqlx::query_scalar(per_table!(
            p.kind,
            "UPDATE ",
            " SET deleted_at = NULL WHERE id = $1 AND deleted_at IS NOT NULL RETURNING id"
        ))
        .bind(p.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(error::db)?;
        let how = if undeleted.is_some() {
            "undeleted"
        } else {
            restore_removed_row(&mut tx, p.kind, p.id).await?;
            "reinserted from history"
        };
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "restored": { "kind": p.kind, "id": p.id }, "how": how }))
    }
}

/// Puts back a row that a client removed outright, as it was just before the
/// removal but live: a row soft deleted before it was removed would otherwise
/// come back still deleted.
async fn restore_removed_row(tx: &mut PgConnection, kind: Kind, id: i64) -> Result<(), ErrorData> {
    let live: Option<i32> = sqlx::query_scalar(per_table!(kind, "SELECT 1 FROM ", " WHERE id = $1"))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(error::db)?;
    if live.is_some() {
        return Err(error::invalid("id", "is not deleted"));
    }
    let before: Option<Value> = sqlx::query_scalar(
        "SELECT before FROM change WHERE table_name = $1 AND row_id = $2 AND operation = 'DELETE'
         ORDER BY id DESC LIMIT 1",
    )
    .bind(kind.table())
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(error::db)?;
    let before = before.ok_or_else(|| error::invalid("id", "no recorded row of this kind with this id"))?;
    sqlx::query(per_table!(
        kind,
        "INSERT INTO ",
        " OVERRIDING SYSTEM VALUE SELECT * FROM jsonb_populate_record(NULL::",
        ", $1 || '{\"deleted_at\": null}'::jsonb)"
    ))
    .bind(before)
    .execute(&mut *tx)
    .await
    .map_err(error::db)?;
    Ok(())
}

pub struct NewFile<'a> {
    pub name: &'a str,
    pub media_type: Option<&'a str>,
    pub bytes: &'a [u8],
    pub links: &'a Links,
}

/// Stores a file's bytes and records it. Shared by the tool and the HTTP
/// upload routes, so a file arrives the same way whichever path it took.
pub async fn store_file(
    pool: &PgPool,
    files: &FileStore,
    author: &str,
    reason: &str,
    file: NewFile<'_>,
) -> Result<Value, ErrorData> {
    let reason = super::reason(reason)?;
    let name = file.name.trim();
    if name.is_empty() || name.chars().count() > 255 || name.chars().any(char::is_control) {
        return Err(error::invalid(
            "name",
            "must be 1 to 255 characters with no control characters",
        ));
    }
    let media_type = match file.media_type {
        Some(given) => check_media_type(given)?,
        None => guess_media_type(name).to_owned(),
    };
    let sha = files
        .put(file.bytes)
        .await
        .map_err(|e| error::internal("storing a file", e))?;
    let mut tx = db::begin_write(pool, author, reason).await.map_err(error::db)?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO file (name, media_type, size, sha256, site_id, floor_id, space_id, object_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(name)
    .bind(&media_type)
    .bind(file.bytes.len() as i64)
    .bind(sha.as_str())
    .bind(file.links.site_id)
    .bind(file.links.floor_id)
    .bind(file.links.space_id)
    .bind(file.links.object_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(error::db)?;
    tx.commit().await.map_err(error::db)?;
    Ok(json!({ "id": id, "name": name, "media_type": media_type, "size": file.bytes.len(), "sha256": sha.as_str() }))
}

fn check_media_type(value: &str) -> Result<String, ErrorData> {
    let value = value.trim().to_ascii_lowercase();
    let valid = value.len() <= 255
        && value.split_once('/').is_some_and(|(a, b)| {
            let part = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b".+-_".contains(&c));
            part(a) && part(b)
        });
    if valid {
        Ok(value)
    } else {
        Err(error::invalid("media_type", "not a media type such as text/plain"))
    }
}

fn guess_media_type(name: &str) -> &'static str {
    let extension = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match extension.as_deref() {
        Some("dxf") => "image/vnd.dxf",
        Some("dwg") => "image/vnd.dwg",
        Some("geojson") => "application/geo+json",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("heic") => "image/heic",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain",
        Some("md") => "text/markdown",
        Some("csv") => "text/csv",
        _ => "application/octet-stream",
    }
}

/// Refuses a create that leaves out a field only an update may leave out,
/// naming the first one missing.
fn required_on_create<const N: usize>(what: &str, fields: [(&str, bool); N]) -> Result<(), ErrorData> {
    match fields.into_iter().find(|(_, given)| !given) {
        Some((field, _)) => Err(error::invalid(
            field,
            format!("is required to create a {what}; to change an existing {what}, give its id"),
        )),
        None => Ok(()),
    }
}

pub(super) fn check_space_kind(field: &str, kind: &str) -> Result<(), ErrorData> {
    if SPACE_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(error::invalid(
            field,
            format!("must be one of {}", SPACE_KINDS.join(", ")),
        ))
    }
}

/// An object kind in its stored form: lowercase, words joined by underscores.
pub(super) fn object_kind(field: &str, kind: &str) -> Result<String, ErrorData> {
    let normal: String = kind
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c == ' ' || c == '-' { '_' } else { c })
        .collect();
    let valid = normal.len() <= 40
        && normal.starts_with(|c: char| c.is_ascii_lowercase())
        && normal
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(normal)
    } else {
        Err(error::invalid(
            field,
            "must be a word of up to 40 letters, digits and underscores",
        ))
    }
}

fn check_key(field: &str, key: &str) -> Result<(), ErrorData> {
    let valid = (1..=64).contains(&key.len())
        && key.starts_with(|c: char| c.is_ascii_lowercase())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(error::invalid(
            field,
            "keys are lowercase letters, digits and underscores",
        ))
    }
}

/// Refuses an EPSG code PostGIS does not know before it reaches ST_Transform.
pub(super) async fn check_epsg(tx: &mut PgConnection, field: &str, epsg: i32) -> Result<(), ErrorData> {
    let known: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM spatial_ref_sys WHERE srid = $1)")
        .bind(epsg)
        .fetch_one(&mut *tx)
        .await
        .map_err(error::db)?;
    if known {
        Ok(())
    } else {
        Err(error::invalid(
            field,
            format!("EPSG:{epsg} is not a coordinate system PostGIS knows"),
        ))
    }
}

/// Asks PostGIS whether a caller's geometry is of an allowed type, within the
/// vertex limit and valid.
pub(super) async fn check_geometry(
    tx: &mut PgConnection,
    field: &str,
    wkb: &[u8],
    allowed: &[&str],
) -> Result<(), ErrorData> {
    let (kind, points, empty, reason): (String, i32, bool, Option<String>) = sqlx::query_as(
        "SELECT GeometryType(g), ST_NPoints(g), ST_IsEmpty(g), CASE WHEN ST_IsValid(g) THEN NULL ELSE ST_IsValidReason(g) END
         FROM ST_GeomFromWKB($1, 0) AS g",
    )
    .bind(wkb)
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| error::invalid(field, "not a geometry PostGIS can read"))?;
    if !allowed.contains(&kind.as_str()) {
        return Err(error::invalid(
            field,
            format!("is a {kind}; expected one of {}", allowed.join(", ")),
        ));
    }
    if empty {
        return Err(error::invalid(field, "is empty"));
    }
    if usize::try_from(points).unwrap_or(usize::MAX) > MAX_VERTICES {
        return Err(error::invalid(field, format!("has more than {MAX_VERTICES} vertices")));
    }
    if let Some(reason) = reason {
        return Err(error::invalid(field, format!("is not valid: {reason}")));
    }
    Ok(())
}

/// An origin that PostGIS cannot project into UTM zone 32N, or that lands
/// outside the zone, names the field rather than failing as a server error.
fn origin_error(error: sqlx::Error, origin: Option<&Origin>) -> ErrorData {
    let database = error.as_database_error();
    let outside = database.and_then(|e| e.constraint()) == Some("site_origin_in_zone");
    let unprojectable = database.and_then(|e| e.code()).is_some_and(|code| code == "XX000");
    match origin {
        Some(origin) if outside || unprojectable => error::invalid(
            "origin",
            format!("cannot be placed in UTM zone 32N from EPSG:{}", origin.epsg),
        ),
        _ => error::db(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_kinds_are_normalised() {
        assert_eq!(
            object_kind("kind", " Access Point ").ok().as_deref(),
            Some("access_point")
        );
        assert_eq!(object_kind("kind", "cable-run").ok().as_deref(), Some("cable_run"));
        assert!(object_kind("kind", "9lives").is_err());
        assert!(object_kind("kind", "light; drop table").is_err());
    }

    #[test]
    fn media_types_are_checked() {
        assert_eq!(check_media_type("Image/PNG").ok().as_deref(), Some("image/png"));
        assert!(check_media_type("text/html; charset=utf-8").is_err());
        assert!(check_media_type("nonsense").is_err());
    }
}

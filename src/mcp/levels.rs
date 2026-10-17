//! The levels below an object and above a site.
//!
//! A detail is a drawing of an object, or of part of another detail, in its
//! own frame: millimetres, x right, y up, origin at the drawing's lower left,
//! with no relation to the site frame. An area is a neighbourhood, town or
//! region a site is in, with an optional outline in WGS 84. `context` walks
//! from any row up to the outermost area and lists what is directly inside
//! it.
//!
//! Every read of a detail, part or area goes through `live_detail`,
//! `live_detail_part` and `live_area`, and no floor or site query reads these
//! tables.

use axum::http::request::Parts;
use rmcp::{
    ErrorData,
    handler::server::{tool::Extension, wrapper::Parameters},
    model::CallToolResult,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, Row, postgres::PgRow};

use super::{
    House, geojson, json_result, reason, wkb,
    write::{
        OBJECT_GEOMETRY_TYPES, check_epsg, check_geometry, check_live_area, object_kind, present, required_on_create,
    },
    writer,
};
use crate::{db, error};

/// The most items `context` lists of one kind; `count` gives them all.
const DOWN_ITEMS: i64 = 50;

/// How a detail looks at what it draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Top,
    Front,
    Side,
    Section,
    Exploded,
    Other,
}

impl View {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Front => "front",
            Self::Side => "side",
            Self::Section => "section",
            Self::Exploded => "exploded",
            Self::Other => "other",
        }
    }
}

/// How a measurement was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Measured on the thing itself.
    Measured,
    /// Read off a drawing.
    FromDrawing,
    /// A guess.
    Estimated,
}

/// One measurement in millimetres and how it was found.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    /// Millimetres, above zero.
    pub value: f64,
    pub source: Source,
}

/// A detail's overall size in millimetres; each is optional.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Measurements {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<Measurement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<Measurement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<Measurement>,
}

/// Where a detail's origin lies in its parent's frame and units: metres in
/// the site frame for a detail of an object, millimetres for a detail inside
/// another detail.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DetailPlacement {
    pub x: f64,
    pub y: f64,
    /// Degrees counterclockwise. Default 0.
    #[serde(default)]
    pub rotation: f64,
    /// Parent units per millimetre of this detail. Default 1.
    #[serde(default = "one")]
    pub scale: f64,
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpsertDetailParams {
    /// Omit to create a detail; give to update one, when every field left
    /// out keeps its stored value.
    pub id: Option<i64>,
    /// The object the detail draws. Give this or `parent_detail_id` on
    /// create; give either on update to move the detail.
    pub object_id: Option<i64>,
    /// The detail this one is a part of, such as a drawer in a desk.
    pub parent_detail_id: Option<i64>,
    /// Required on create.
    pub name: Option<String>,
    /// `top`, `front`, `side`, `section`, `exploded` or `other`. Required on
    /// create.
    pub view: Option<View>,
    /// Overall size in millimetres, each of `width`, `depth` and `height`
    /// optional and each `{"value": .., "source": "measured" | "from_drawing"
    /// | "estimated"}`. On update, left out keeps them and null clears them.
    #[serde(default, deserialize_with = "present")]
    pub measurements: Option<Option<Measurements>>,
    /// Where the detail lies in its parent: `{"x", "y", "rotation", "scale"}`.
    /// Without it the detail is linked to its parent but not drawn in it. On
    /// update, left out keeps it and null clears it.
    #[serde(default, deserialize_with = "present")]
    pub placement: Option<Option<DetailPlacement>>,
    /// Replaces the attributes when given.
    pub attributes: Option<Map<String, Value>>,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpsertDetailPartParams {
    /// Omit to create a part; give to update one, when every field left out
    /// keeps its stored value.
    pub id: Option<i64>,
    /// The detail the part belongs to. Required on create.
    pub detail_id: Option<i64>,
    /// Such as `top` or `left leg`. Required on create.
    pub name: Option<String>,
    /// GeoJSON Point, LineString or Polygon (or a Multi of one) in the
    /// detail's frame, millimetres. Required on create.
    pub geometry: Option<Value>,
    /// Replaces the attributes when given: a material, a finish, a note.
    pub attributes: Option<Map<String, Value>>,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpsertAreaParams {
    /// Omit to create an area; give to update one, when every field left
    /// out keeps its stored value.
    pub id: Option<i64>,
    /// Required on create.
    pub name: Option<String>,
    /// `neighbourhood`, `hamlet`, `town`, `municipality`, `valley`, `region`
    /// or another lowercase word. Required on create.
    pub kind: Option<String>,
    /// The area this one is inside. On update, left out keeps it and null
    /// clears it.
    #[serde(default, deserialize_with = "present")]
    pub parent_area_id: Option<Option<i64>>,
    /// The outline: a GeoJSON Polygon or MultiPolygon in `epsg`, stored in
    /// WGS 84. On update, left out keeps it and null clears it.
    #[serde(default, deserialize_with = "present")]
    pub geometry: Option<Option<Value>>,
    /// EPSG code of `geometry`. Default 4326, longitude and latitude.
    pub epsg: Option<i32>,
    /// Replaces the attributes when given.
    pub attributes: Option<Map<String, Value>>,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetSiteAreaParams {
    pub site_id: i64,
    /// The area the site is in, or null to clear it, after which the site is
    /// in the smallest area whose outline covers its origin. Required.
    #[serde(deserialize_with = "nullable")]
    pub area_id: Option<i64>,
    pub reason: String,
}

/// A field that must be given but may be null.
fn nullable<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    Option::<i64>::deserialize(deserializer)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDetailParams {
    pub detail_id: i64,
}

/// Exactly one of these.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContextParams {
    pub site_id: Option<i64>,
    pub floor_id: Option<i64>,
    pub space_id: Option<i64>,
    pub object_id: Option<i64>,
    pub detail_id: Option<i64>,
    pub area_id: Option<i64>,
}

#[tool_router(router = levels_write_router, vis = "pub(super)")]
impl House {
    #[tool(
        description = "Create a detail, or update one when `id` is given. A detail is a drawing of an object, such as a table seen from the top, or of part of another detail, such as a drawer inside that table, nested to any depth. It has its own frame: millimetres, x right, y up, origin at the drawing's lower left, with no relation to the site frame, so floor plans and spatial tools (what_is_at, nearest, in_space, measure) never see it. On create give exactly one parent, `object_id` or `parent_detail_id`, and `name` and `view` (`top`, `front`, `side`, `section`, `exploded` or `other`). `measurements` are optional, each of `width`, `depth` and `height` in millimetres with its `source`: `measured`, `from_drawing` or `estimated`. `placement` ({\"x\", \"y\", \"rotation\", \"scale\"}, in the parent's frame and units) is optional: without it the detail is linked to its parent but not drawn in it. On update a field left out keeps its stored value; send `measurements` or `placement` as null to clear it. Draw it with render_plan {\"detail_id\": ..}; add labelled parts with upsert_detail_part, linework with import_dxf {\"detail_id\": ..}, and photos or quotes with upload_file {\"links\": {\"detail_id\": ..}}. Example: {\"object_id\": 41, \"name\": \"Top\", \"view\": \"top\", \"measurements\": {\"width\": {\"value\": 2400, \"source\": \"measured\"}, \"height\": {\"value\": 720, \"source\": \"estimated\"}}, \"reason\": \"maker's drawing\"}."
    )]
    async fn upsert_detail(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpsertDetailParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        if p.object_id.is_some() && p.parent_detail_id.is_some() {
            return Err(error::invalid(
                "parent_detail_id",
                "give one parent, object_id or parent_detail_id, not both",
            ));
        }
        let moves = p.object_id.is_some() || p.parent_detail_id.is_some();
        if p.id.is_none() {
            if !moves {
                return Err(error::invalid(
                    "object_id",
                    "a new detail needs a parent: object_id or parent_detail_id",
                ));
            }
            required_on_create("detail", [("name", p.name.is_some()), ("view", p.view.is_some())])?;
        }
        if p.parent_detail_id.is_some() && p.parent_detail_id == p.id {
            return Err(error::invalid("parent_detail_id", "a detail cannot be inside itself"));
        }
        let name = p.name.as_deref().map(|n| level_name("name", n)).transpose()?;
        let measurements = match &p.measurements {
            Some(Some(m)) => Some(measurements_json(m)?),
            _ => None,
        };
        let placement = match &p.placement {
            Some(Some(placement)) => Some(placement_json(placement)?),
            _ => None,
        };
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        let site = if moves {
            Some(parent_site(&mut tx, p.object_id, p.parent_detail_id).await?)
        } else {
            None
        };
        let id: Option<i64> = sqlx::query_scalar(
            "WITH created AS (
                 INSERT INTO detail (site_id, object_id, parent_detail_id, name, view, measurements, placement, attributes)
                 SELECT $2, $3, $4, $5, $6, $7, $8, coalesce($9, '{}')
                 WHERE $1::bigint IS NULL RETURNING id
             ), updated AS (
                 UPDATE detail SET site_id = coalesce($2, site_id),
                                   object_id = CASE WHEN $10 THEN $3 ELSE object_id END,
                                   parent_detail_id = CASE WHEN $10 THEN $4 ELSE parent_detail_id END,
                                   name = coalesce($5, name), view = coalesce($6, view),
                                   measurements = CASE WHEN $11 THEN $7 ELSE measurements END,
                                   placement = CASE WHEN $12 THEN $8 ELSE placement END,
                                   attributes = coalesce($9, attributes)
                 WHERE id = $1 AND id IN (SELECT id FROM live_detail) RETURNING id
             )
             SELECT id FROM created UNION ALL SELECT id FROM updated",
        )
        .bind(p.id)
        .bind(site)
        .bind(p.object_id)
        .bind(p.parent_detail_id)
        .bind(name)
        .bind(p.view.map(View::as_str))
        .bind(measurements)
        .bind(placement)
        .bind(p.attributes.map(Value::Object))
        .bind(moves)
        .bind(p.measurements.is_some())
        .bind(p.placement.is_some())
        .fetch_optional(&mut *tx)
        .await
        .map_err(level_error)?;
        let id = id.ok_or_else(|| error::invalid("id", "no live detail with this id"))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": id }))
    }

    #[tool(
        description = "Create a labelled part of a detail, or update one when `id` is given: a top, a leg, a drawer front, a hinge. Its geometry is a GeoJSON Point, LineString or Polygon (or a Multi of one) in the detail's frame, millimetres, x right, y up from the drawing's lower left. Attributes suit a material, a finish or a note, such as {\"material\": \"oak\", \"finish\": \"oiled\"}. render_plan {\"detail_id\": ..} draws and labels the parts by the same rules as rooms on a plan. On create `detail_id`, `name` and `geometry` are required; on update a field left out keeps its stored value. Example: {\"detail_id\": 4, \"name\": \"left leg\", \"geometry\": {\"type\": \"Polygon\", \"coordinates\": [[[50, 50], [120, 50], [120, 550], [50, 550], [50, 50]]]}, \"attributes\": {\"material\": \"oak\"}, \"reason\": \"from the drawing\"}."
    )]
    async fn upsert_detail_part(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpsertDetailPartParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        if p.id.is_none() {
            required_on_create(
                "part",
                [
                    ("detail_id", p.detail_id.is_some()),
                    ("name", p.name.is_some()),
                    ("geometry", p.geometry.is_some()),
                ],
            )?;
        }
        let name = p.name.as_deref().map(|n| level_name("name", n)).transpose()?;
        let geometry = p.geometry.as_ref().map(|g| wkb("geometry", g)).transpose()?;
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        if let Some(detail) = p.detail_id {
            check_live_detail(&mut tx, "detail_id", detail).await?;
        }
        if let Some(geometry) = &geometry {
            check_geometry(&mut tx, "geometry", geometry, OBJECT_GEOMETRY_TYPES).await?;
        }
        let id: Option<i64> = sqlx::query_scalar(
            "WITH created AS (
                 INSERT INTO detail_part (detail_id, name, geom, attributes)
                 SELECT $2, $3, ST_GeomFromWKB($4, 0), coalesce($5, '{}')
                 WHERE $1::bigint IS NULL RETURNING id
             ), updated AS (
                 UPDATE detail_part SET detail_id = coalesce($2, detail_id), name = coalesce($3, name),
                                        geom = coalesce(ST_GeomFromWKB($4, 0), geom),
                                        attributes = coalesce($5, attributes)
                 WHERE id = $1 AND id IN (SELECT id FROM live_detail_part) RETURNING id
             )
             SELECT id FROM created UNION ALL SELECT id FROM updated",
        )
        .bind(p.id)
        .bind(p.detail_id)
        .bind(name)
        .bind(&geometry)
        .bind(p.attributes.map(Value::Object))
        .fetch_optional(&mut *tx)
        .await
        .map_err(error::db)?;
        let id = id.ok_or_else(|| error::invalid("id", "no live part with this id"))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": id }))
    }

    #[tool(
        description = "Create an area around sites, or update one when `id` is given: a neighbourhood, hamlet, town, municipality, valley or region (`kind`, any lowercase word), optionally inside another area (`parent_area_id`), nested to any depth. `geometry` is an optional outline, a GeoJSON Polygon or MultiPolygon in any EPSG coordinate system (`epsg`, default 4326), stored in WGS 84. A site is in an area when its `area_id` names it (create_site, set_site_area), or, with no `area_id`, when its origin falls inside the area's outline; context and list_sites say which rule applied. import_geojson {\"as\": \"area\"} brings a geoportal boundary in directly. Draw an area with render_plan {\"area_id\": ..}. On create `name` and `kind` are required; on update a field left out keeps its stored value, and `parent_area_id` or `geometry` sent as null clears it. Example: {\"name\": \"Old town\", \"kind\": \"neighbourhood\", \"parent_area_id\": 2, \"geometry\": {\"type\": \"Polygon\", \"coordinates\": [[[9.18, 45.46], [9.19, 45.46], [9.19, 45.47], [9.18, 45.46]]]}, \"reason\": \"from the town's map\"}."
    )]
    async fn upsert_area(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpsertAreaParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        if p.id.is_none() {
            required_on_create("area", [("name", p.name.is_some()), ("kind", p.kind.is_some())])?;
        }
        let name = p.name.as_deref().map(|n| level_name("name", n)).transpose()?;
        let kind = p.kind.as_deref().map(|k| object_kind("kind", k)).transpose()?;
        let parent = p.parent_area_id.flatten();
        if parent.is_some() && parent == p.id {
            return Err(error::invalid("parent_area_id", "an area cannot be inside itself"));
        }
        let geometry = match &p.geometry {
            Some(Some(g)) => Some(wkb("geometry", g)?),
            _ => None,
        };
        let epsg = p.epsg.unwrap_or(4326);
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        if let Some(parent) = parent {
            check_live_area(&mut tx, "parent_area_id", parent).await?;
        }
        let outline = match &geometry {
            Some(geometry) => {
                check_epsg(&mut tx, "epsg", epsg).await?;
                check_geometry(&mut tx, "geometry", geometry, &["POLYGON", "MULTIPOLYGON"]).await?;
                Some(to_wgs84(&mut tx, geometry, epsg).await?)
            }
            None => None,
        };
        let id: Option<i64> = sqlx::query_scalar(
            "WITH created AS (
                 INSERT INTO area (name, kind, parent_area_id, geom, attributes)
                 SELECT $2, $3, $4, ST_GeomFromWKB($5, 4326), coalesce($6, '{}')
                 WHERE $1::bigint IS NULL RETURNING id
             ), updated AS (
                 UPDATE area SET name = coalesce($2, name), kind = coalesce($3, kind),
                                 parent_area_id = CASE WHEN $7 THEN $4 ELSE parent_area_id END,
                                 geom = CASE WHEN $8 THEN ST_GeomFromWKB($5, 4326) ELSE geom END,
                                 attributes = coalesce($6, attributes)
                 WHERE id = $1 AND id IN (SELECT id FROM live_area) RETURNING id
             )
             SELECT id FROM created UNION ALL SELECT id FROM updated",
        )
        .bind(p.id)
        .bind(name)
        .bind(kind)
        .bind(parent)
        .bind(&outline)
        .bind(p.attributes.map(Value::Object))
        .bind(p.parent_area_id.is_some())
        .bind(p.geometry.is_some())
        .fetch_optional(&mut *tx)
        .await
        .map_err(level_error)?;
        let id = id.ok_or_else(|| error::invalid("id", "no live area with this id"))?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "id": id }))
    }

    #[tool(
        description = "Set or clear the area a site is in: `area_id` names an area from upsert_area, such as the site's neighbourhood, or null clears it, after which the site is in the smallest area whose outline covers its origin, if any. The site's origin and geometry do not change; set_site_origin is about the origin. Returns the area the site is now in, with `via`: `area_id` or `origin`. Example: {\"site_id\": 3, \"area_id\": 7, \"reason\": \"the site is in this hamlet\"}."
    )]
    async fn set_site_area(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SetSiteAreaParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        if let Some(area) = p.area_id {
            check_live_area(&mut tx, "area_id", area).await?;
        }
        let updated: Option<i64> =
            sqlx::query_scalar("UPDATE site SET area_id = $2 WHERE id = $1 AND deleted_at IS NULL RETURNING id")
                .bind(p.site_id)
                .bind(p.area_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(error::db)?;
        updated.ok_or_else(|| error::invalid("site_id", "no live site with this id"))?;
        let area = site_area(&mut tx, p.site_id).await?;
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "site_id": p.site_id, "area": area }))
    }
}

#[tool_router(router = levels_read_router, vis = "pub(super)")]
impl House {
    #[tool(
        description = "A detail (a drawing of an object or of part of another detail, in its own millimetre frame) with its parts and their geometry in that frame, its measurements and how each was found (`measured`, `from_drawing` or `estimated`), its placement in its parent (null when it is linked but not drawn there), its linked files, the details directly inside it (id, name, view, placed), and `path`, the chain up to its object, from this detail first. A detail is live only while it, every detail it is inside, and its object, floor and site are live. render_plan {\"detail_id\": ..} draws it; context {\"detail_id\": ..} walks on up to the site and its areas."
    )]
    async fn get_detail(&self, Parameters(p): Parameters<GetDetailParams>) -> Result<CallToolResult, ErrorData> {
        let row = sqlx::query(
            "SELECT id, site_id, object_id, parent_detail_id, name, view, measurements, placement, attributes, created_at,
                    (SELECT count(*) FROM detail_linework l WHERE l.detail_id = d.id AND l.deleted_at IS NULL) AS linework
             FROM live_detail d WHERE id = $1",
        )
        .bind(p.detail_id)
        .fetch_optional(&self.db)
        .await
        .map_err(error::db)?
        .ok_or_else(|| error::invalid("detail_id", "no live detail with this id"))?;
        let parts = sqlx::query(
            "SELECT id, name, attributes, ST_AsBinary(geom) AS wkb FROM live_detail_part WHERE detail_id = $1 ORDER BY name, id",
        )
        .bind(p.detail_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let mut part_values = Vec::with_capacity(parts.len());
        for r in &parts {
            part_values.push(json!({
                "id": r.get::<i64, _>("id"),
                "name": r.get::<String, _>("name"),
                "attributes": r.get::<Value, _>("attributes"),
                "geometry": geojson(&r.get::<Vec<u8>, _>("wkb"))?,
            }));
        }
        let children = sqlx::query(
            "SELECT id, name, view, placement IS NOT NULL AS placed FROM live_detail WHERE parent_detail_id = $1 ORDER BY name, id",
        )
        .bind(p.detail_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let files = sqlx::query(
            "SELECT f.id, f.name, f.media_type, f.size, f.sha256, f.created_at
             FROM file f JOIN file_link k ON k.file_id = f.id
             WHERE k.detail_id = $1 AND f.deleted_at IS NULL ORDER BY f.created_at DESC, f.id DESC",
        )
        .bind(p.detail_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let path = self.detail_path(p.detail_id).await?;
        json_result(json!({
            "id": row.get::<i64, _>("id"),
            "site_id": row.get::<i64, _>("site_id"),
            "object_id": row.get::<Option<i64>, _>("object_id"),
            "parent_detail_id": row.get::<Option<i64>, _>("parent_detail_id"),
            "name": row.get::<String, _>("name"),
            "view": row.get::<String, _>("view"),
            "frame": "millimetres, x right, y up, origin at the drawing's lower left",
            "measurements": row.get::<Option<Value>, _>("measurements"),
            "placement": row.get::<Option<Value>, _>("placement"),
            "attributes": row.get::<Value, _>("attributes"),
            "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            "linework": row.get::<i64, _>("linework"),
            "parts": part_values,
            "details": children.iter().map(|r| json!({
                "id": r.get::<i64, _>("id"),
                "name": r.get::<String, _>("name"),
                "view": r.get::<String, _>("view"),
                "placed": r.get::<bool, _>("placed"),
            })).collect::<Vec<_>>(),
            "files": files.iter().map(|r| json!({
                "id": r.get::<i64, _>("id"),
                "name": r.get::<String, _>("name"),
                "media_type": r.get::<String, _>("media_type"),
                "size": r.get::<i64, _>("size"),
                "sha256": r.get::<String, _>("sha256"),
                "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            })).collect::<Vec<_>>(),
            "path": path,
        }))
    }

    #[tool(
        description = "Where anything sits in the model's levels, and what is directly inside it. The levels, outermost first: areas (region, town, neighbourhood, nested to any depth), site, floor, space, object, then details (drawings of the object in their own millimetre frame, nested to any depth). Give exactly one of `area_id`, `site_id`, `floor_id`, `space_id`, `object_id` or `detail_id`. `up` is the chain from that row to the outermost area, one entry per level ({kind, id, name, placed}): a detail goes through the details it is inside, then its object, the smallest space covering the object, its floor, the site, and the areas around the site. A site is in the area its `area_id` names, or, with none, in the smallest area whose outline covers its origin; that area's entry says which with `via` (`area_id` or `origin`). `placed` says whether the step is located within its parent: a detail with a placement, an object not `unplaced`, a georeferenced site, an area with an outline. `down` lists the direct children by kind, each as {count, items}, with at most 50 items. Use it before drawing: render_plan takes `area_id`, `site_id` (with or without `floor_id`) or `detail_id`. Example: {\"detail_id\": 9}."
    )]
    async fn context(&self, Parameters(p): Parameters<ContextParams>) -> Result<CallToolResult, ErrorData> {
        let given = [
            ("area_id", p.area_id),
            ("site_id", p.site_id),
            ("floor_id", p.floor_id),
            ("space_id", p.space_id),
            ("object_id", p.object_id),
            ("detail_id", p.detail_id),
        ];
        let mut named = given.iter().filter_map(|(field, id)| id.map(|id| (*field, id)));
        let (field, id) = named.next().ok_or_else(|| {
            error::invalid(
                "area_id",
                "give one of area_id, site_id, floor_id, space_id, object_id and detail_id",
            )
        })?;
        if let Some((other, _)) = named.next() {
            return Err(error::invalid(
                other,
                format!("give only one row; {field} is already given"),
            ));
        }
        let (kind, up, down) = match field {
            "detail_id" => ("detail", self.up_from_detail(id).await?, self.down_detail(id).await?),
            "object_id" => ("object", self.up_from_object(id).await?, self.down_object(id).await?),
            "space_id" => ("space", self.up_from_space(id).await?, self.down_space(id).await?),
            "floor_id" => ("floor", self.up_from_floor(id).await?, self.down_floor(id).await?),
            "site_id" => ("site", self.up_from_site(id).await?, self.down_site(id).await?),
            _ => ("area", self.up_from_area(id, None).await?, self.down_area(id).await?),
        };
        json_result(json!({ "kind": kind, "id": id, "up": up, "down": down }))
    }
}

impl House {
    /// The details from this one up to its object, then the object.
    pub(super) async fn detail_path(&self, detail_id: i64) -> Result<Vec<Value>, ErrorData> {
        let (mut path, object_id) = self.detail_chain(detail_id).await?;
        let object = sqlx::query("SELECT id, name, kind, placement FROM live_object WHERE id = $1")
            .bind(object_id)
            .fetch_one(&self.db)
            .await
            .map_err(error::db)?;
        path.push(object_step(&object));
        Ok(path)
    }

    /// The live details from this one up through its parents, and the object
    /// at the top. Refused when the detail is not live.
    async fn detail_chain(&self, detail_id: i64) -> Result<(Vec<Value>, i64), ErrorData> {
        let rows = sqlx::query(
            "WITH RECURSIVE up(id, depth) AS (
                 SELECT $1::bigint, 0
                 UNION ALL
                 SELECT d.parent_detail_id, up.depth + 1 FROM detail d JOIN up ON d.id = up.id
                 WHERE d.parent_detail_id IS NOT NULL AND up.depth < 10000
             )
             SELECT d.id, d.name, d.view, d.placement IS NOT NULL AS placed, d.object_id
             FROM up JOIN live_detail d ON d.id = up.id ORDER BY up.depth",
        )
        .bind(detail_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let object_id = rows
            .last()
            .and_then(|r| r.get::<Option<i64>, _>("object_id"))
            .ok_or_else(|| error::invalid("detail_id", "no live detail with this id"))?;
        let steps = rows
            .iter()
            .map(|r| {
                json!({
                    "kind": "detail",
                    "id": r.get::<i64, _>("id"),
                    "name": r.get::<String, _>("name"),
                    "view": r.get::<String, _>("view"),
                    "placed": r.get::<bool, _>("placed"),
                })
            })
            .collect();
        Ok((steps, object_id))
    }

    async fn up_from_detail(&self, id: i64) -> Result<Vec<Value>, ErrorData> {
        let (mut up, object_id) = self.detail_chain(id).await?;
        up.extend(self.up_from_object(object_id).await?);
        Ok(up)
    }

    async fn up_from_object(&self, id: i64) -> Result<Vec<Value>, ErrorData> {
        let object = sqlx::query(concat!(
            "SELECT o.id, o.name, o.kind, o.placement, o.floor_id, o.site_id, within.in_space_id FROM live_object o",
            in_space_join!("o"),
            "WHERE o.id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.db)
        .await
        .map_err(error::db)?
        .ok_or_else(|| error::invalid("object_id", "no live object with this id"))?;
        let mut up = vec![object_step(&object)];
        match object.get::<Option<i64>, _>("in_space_id") {
            Some(space) => up.extend(self.up_from_space(space).await?),
            None => match object.get::<Option<i64>, _>("floor_id") {
                Some(floor) => up.extend(self.up_from_floor(floor).await?),
                None => up.extend(self.up_from_site(object.get("site_id")).await?),
            },
        }
        Ok(up)
    }

    async fn up_from_space(&self, id: i64) -> Result<Vec<Value>, ErrorData> {
        let space = sqlx::query("SELECT id, name, kind, floor_id, site_id FROM live_space WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.db)
            .await
            .map_err(error::db)?
            .ok_or_else(|| error::invalid("space_id", "no live space with this id"))?;
        let mut up = vec![json!({
            "kind": "space",
            "id": space.get::<i64, _>("id"),
            "name": space.get::<String, _>("name"),
            "space_kind": space.get::<String, _>("kind"),
            "placed": true,
        })];
        match space.get::<Option<i64>, _>("floor_id") {
            Some(floor) => up.extend(self.up_from_floor(floor).await?),
            None => up.extend(self.up_from_site(space.get("site_id")).await?),
        }
        Ok(up)
    }

    async fn up_from_floor(&self, id: i64) -> Result<Vec<Value>, ErrorData> {
        let floor = sqlx::query("SELECT id, name, level, site_id FROM live_floor WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.db)
            .await
            .map_err(error::db)?
            .ok_or_else(|| error::invalid("floor_id", "no live floor with this id"))?;
        let mut up = vec![json!({
            "kind": "floor",
            "id": floor.get::<i64, _>("id"),
            "name": floor.get::<String, _>("name"),
            "level": floor.get::<i32, _>("level"),
            "placed": true,
        })];
        up.extend(self.up_from_site(floor.get("site_id")).await?);
        Ok(up)
    }

    async fn up_from_site(&self, id: i64) -> Result<Vec<Value>, ErrorData> {
        let site = sqlx::query(
            "SELECT s.id, s.name, s.origin IS NOT NULL AS georeferenced, a.area_id, a.via
             FROM site s LEFT JOIN LATERAL house_site_area(s.id) AS a ON true
             WHERE s.id = $1 AND s.deleted_at IS NULL",
        )
        .bind(id)
        .fetch_optional(&self.db)
        .await
        .map_err(error::db)?
        .ok_or_else(|| error::invalid("site_id", "no live site with this id"))?;
        let mut up = vec![json!({
            "kind": "site",
            "id": site.get::<i64, _>("id"),
            "name": site.get::<String, _>("name"),
            "placed": site.get::<bool, _>("georeferenced"),
        })];
        if let Some(area) = site.get::<Option<i64>, _>("area_id") {
            up.extend(self.up_from_area(area, site.get::<Option<String>, _>("via")).await?);
        }
        Ok(up)
    }

    /// The area and every area it is inside, innermost first. `via` is how
    /// the row below reached the first: `area_id` or `origin` for a site.
    async fn up_from_area(&self, id: i64, via: Option<String>) -> Result<Vec<Value>, ErrorData> {
        let rows = sqlx::query(
            "WITH RECURSIVE up(id, depth) AS (
                 SELECT $1::bigint, 0
                 UNION ALL
                 SELECT a.parent_area_id, up.depth + 1 FROM area a JOIN up ON a.id = up.id
                 WHERE a.parent_area_id IS NOT NULL AND up.depth < 10000
             )
             SELECT a.id, a.name, a.kind, a.geom IS NOT NULL AS outlined
             FROM up JOIN live_area a ON a.id = up.id ORDER BY up.depth",
        )
        .bind(id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        if rows.is_empty() {
            return Err(error::invalid("area_id", "no live area with this id"));
        }
        Ok(rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mut step = json!({
                    "kind": "area",
                    "id": r.get::<i64, _>("id"),
                    "name": r.get::<String, _>("name"),
                    "area_kind": r.get::<String, _>("kind"),
                    "placed": r.get::<bool, _>("outlined"),
                });
                if i == 0 {
                    if let Some(via) = &via {
                        step["via"] = json!(via);
                    }
                } else {
                    step["via"] = json!("parent_area_id");
                }
                step
            })
            .collect())
    }

    async fn down_detail(&self, id: i64) -> Result<Value, ErrorData> {
        Ok(json!({
            "details": self.down(
                "SELECT id, name, placement IS NOT NULL AS placed, jsonb_build_object('view', view) AS extra,
                        count(*) OVER () AS total
                 FROM live_detail WHERE parent_detail_id = $1 ORDER BY name, id LIMIT $2",
                id,
            ).await?,
            "parts": self.down(
                "SELECT id, name, true AS placed, '{}'::jsonb AS extra, count(*) OVER () AS total
                 FROM live_detail_part WHERE detail_id = $1 ORDER BY name, id LIMIT $2",
                id,
            ).await?,
        }))
    }

    async fn down_object(&self, id: i64) -> Result<Value, ErrorData> {
        Ok(json!({
            "details": self.down(
                "SELECT id, name, placement IS NOT NULL AS placed, jsonb_build_object('view', view) AS extra,
                        count(*) OVER () AS total
                 FROM live_detail WHERE object_id = $1 ORDER BY name, id LIMIT $2",
                id,
            ).await?,
        }))
    }

    async fn down_space(&self, id: i64) -> Result<Value, ErrorData> {
        Ok(json!({
            "spaces": self.down(
                "SELECT i.id, i.name, true AS placed, jsonb_build_object('space_kind', i.kind) AS extra,
                        count(*) OVER () AS total
                 FROM live_space s JOIN live_space i
                   ON i.site_id = s.site_id AND i.floor_id IS NOT DISTINCT FROM s.floor_id
                  AND i.id <> s.id AND ST_CoveredBy(i.geom, s.geom)
                 WHERE s.id = $1 ORDER BY i.name, i.id LIMIT $2",
                id,
            ).await?,
            "objects": self.down(
                concat!(
                    "SELECT o.id, o.name, true AS placed,
                            jsonb_build_object('object_kind', o.kind, 'placement', o.placement) AS extra,
                            count(*) OVER () AS total
                     FROM live_space s JOIN live_object o ON o.site_id = s.site_id AND o.floor_id IS NOT DISTINCT FROM s.floor_id",
                    in_space_join!("o"),
                    "WHERE s.id = $1 AND within.in_space_id = s.id ORDER BY o.kind, o.name, o.id LIMIT $2"
                ),
                id,
            ).await?,
        }))
    }

    async fn down_floor(&self, id: i64) -> Result<Value, ErrorData> {
        Ok(json!({
            "spaces": self.down(
                "SELECT id, name, true AS placed, jsonb_build_object('space_kind', kind) AS extra, count(*) OVER () AS total
                 FROM live_space WHERE floor_id = $1 ORDER BY name, id LIMIT $2",
                id,
            ).await?,
            "objects": self.down(
                "SELECT id, name, placement <> 'unplaced' AS placed,
                        jsonb_build_object('object_kind', kind, 'placement', placement) AS extra, count(*) OVER () AS total
                 FROM live_object WHERE floor_id = $1 ORDER BY kind, name, id LIMIT $2",
                id,
            ).await?,
        }))
    }

    async fn down_site(&self, id: i64) -> Result<Value, ErrorData> {
        Ok(json!({
            "floors": self.down(
                "SELECT id, name, true AS placed, jsonb_build_object('level', level) AS extra, count(*) OVER () AS total
                 FROM live_floor WHERE site_id = $1 ORDER BY level, id LIMIT $2",
                id,
            ).await?,
            "spaces": self.down(
                "SELECT id, name, true AS placed, jsonb_build_object('space_kind', kind) AS extra, count(*) OVER () AS total
                 FROM live_space WHERE site_id = $1 AND floor_id IS NULL ORDER BY name, id LIMIT $2",
                id,
            ).await?,
            "objects": self.down(
                "SELECT id, name, placement <> 'unplaced' AS placed,
                        jsonb_build_object('object_kind', kind, 'placement', placement) AS extra, count(*) OVER () AS total
                 FROM live_object WHERE site_id = $1 AND floor_id IS NULL ORDER BY kind, name, id LIMIT $2",
                id,
            ).await?,
        }))
    }

    async fn down_area(&self, id: i64) -> Result<Value, ErrorData> {
        Ok(json!({
            "areas": self.down(
                "SELECT id, name, geom IS NOT NULL AS placed, jsonb_build_object('area_kind', kind) AS extra,
                        count(*) OVER () AS total
                 FROM live_area WHERE parent_area_id = $1 ORDER BY name, id LIMIT $2",
                id,
            ).await?,
            "sites": self.down(
                "SELECT s.id, s.name, s.origin IS NOT NULL AS placed, jsonb_build_object('via', a.via) AS extra,
                        count(*) OVER () AS total
                 FROM site s CROSS JOIN LATERAL house_site_area(s.id) AS a
                 WHERE s.deleted_at IS NULL AND a.area_id = $1 ORDER BY s.name, s.id LIMIT $2",
                id,
            ).await?,
        }))
    }

    /// One kind of child: a statement giving `id`, `name`, `placed`, `extra`
    /// (fields merged into each item) and `total`, bound to the parent's id
    /// and the item limit.
    async fn down(&self, sql: &'static str, id: i64) -> Result<Value, ErrorData> {
        let rows = sqlx::query(sql)
            .bind(id)
            .bind(DOWN_ITEMS)
            .fetch_all(&self.db)
            .await
            .map_err(error::db)?;
        let count = rows.first().map_or(0, |r| r.get::<i64, _>("total"));
        let items: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut item = json!({
                    "id": r.get::<i64, _>("id"),
                    "name": r.get::<String, _>("name"),
                    "placed": r.get::<bool, _>("placed"),
                });
                if let Value::Object(extra) = r.get::<Value, _>("extra") {
                    for (key, value) in extra {
                        item[key] = value;
                    }
                }
                item
            })
            .collect();
        let mut group = json!({ "count": count, "items": items });
        if count > DOWN_ITEMS {
            group["truncated"] = json!(true);
        }
        Ok(group)
    }
}

fn object_step(r: &PgRow) -> Value {
    let placement: String = r.get("placement");
    json!({
        "kind": "object",
        "id": r.get::<i64, _>("id"),
        "name": r.get::<String, _>("name"),
        "object_kind": r.get::<String, _>("kind"),
        "placed": placement != "unplaced",
        "placement": placement,
    })
}

/// The area a site is in, with how: `{id, name, kind, via}`, or null.
pub(super) async fn site_area(conn: &mut PgConnection, site_id: i64) -> Result<Value, ErrorData> {
    let row = sqlx::query(
        "SELECT a.id, a.name, a.kind, sa.via FROM house_site_area($1) AS sa JOIN live_area a ON a.id = sa.area_id",
    )
    .bind(site_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(error::db)?;
    Ok(row.map_or(Value::Null, |r| {
        json!({
            "id": r.get::<i64, _>("id"),
            "name": r.get::<String, _>("name"),
            "kind": r.get::<String, _>("kind"),
            "via": r.get::<String, _>("via"),
        })
    }))
}

/// The site of a detail's new parent, refused when the parent is not live.
async fn parent_site(
    tx: &mut PgConnection,
    object_id: Option<i64>,
    parent_detail_id: Option<i64>,
) -> Result<i64, ErrorData> {
    let (sql, field, id) = match (object_id, parent_detail_id) {
        (Some(id), _) => ("SELECT site_id FROM live_object WHERE id = $1", "object_id", id),
        (None, Some(id)) => ("SELECT site_id FROM live_detail WHERE id = $1", "parent_detail_id", id),
        (None, None) => return Err(error::invalid("object_id", "no parent given")),
    };
    let site: Option<i64> = sqlx::query_scalar(sql)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(error::db)?;
    site.ok_or_else(|| {
        error::invalid(
            field,
            if field == "object_id" {
                "no live object with this id"
            } else {
                "no live detail with this id"
            },
        )
    })
}

pub(super) async fn check_live_detail(tx: &mut PgConnection, field: &str, id: i64) -> Result<i64, ErrorData> {
    let site: Option<i64> = sqlx::query_scalar("SELECT site_id FROM live_detail WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(error::db)?;
    site.ok_or_else(|| error::invalid(field, "no live detail with this id"))
}

/// A caller's outline in `epsg` as WKB in WGS 84.
async fn to_wgs84(tx: &mut PgConnection, wkb: &[u8], epsg: i32) -> Result<Vec<u8>, ErrorData> {
    sqlx::query_scalar("SELECT ST_AsBinary(ST_Transform(ST_SetSRID(ST_GeomFromWKB($1), $2), 4326))")
        .bind(wkb)
        .bind(epsg)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| error::invalid("geometry", format!("cannot be converted from EPSG:{epsg} to WGS 84")))
}

/// A name trimmed, refused when blank or longer than 200 characters.
fn level_name(field: &str, name: &str) -> Result<String, ErrorData> {
    let name = name.trim();
    match name.chars().count() {
        0 => Err(error::invalid(field, "is blank")),
        1..=200 => Ok(name.to_owned()),
        _ => Err(error::invalid(field, "is longer than 200 characters")),
    }
}

fn measurements_json(m: &Measurements) -> Result<Value, ErrorData> {
    for (field, value) in [("width", &m.width), ("depth", &m.depth), ("height", &m.height)] {
        if let Some(value) = value
            && !(value.value.is_finite() && value.value > 0.0)
        {
            return Err(error::invalid(
                &format!("measurements.{field}.value"),
                "must be a number of millimetres above zero",
            ));
        }
    }
    serde_json::to_value(m).map_err(|e| error::internal("encoding measurements", e))
}

fn placement_json(p: &DetailPlacement) -> Result<Value, ErrorData> {
    if !(p.x.is_finite() && p.y.is_finite() && p.rotation.is_finite()) {
        return Err(error::invalid("placement", "x, y and rotation must be numbers"));
    }
    if !(p.scale.is_finite() && p.scale > 0.0) {
        return Err(error::invalid("placement.scale", "must be above zero"));
    }
    serde_json::to_value(p).map_err(|e| error::internal("encoding a placement", e))
}

/// The checks the detail and area triggers raise, named for the caller.
fn level_error(e: sqlx::Error) -> ErrorData {
    match e.as_database_error().and_then(|d| d.constraint()) {
        Some("detail_no_cycle") => error::invalid("parent_detail_id", "would put the detail inside itself"),
        Some("detail_same_site") => error::invalid(
            "parent_detail_id",
            "is on another site than the detail, or the detail has details inside it on its present site",
        ),
        Some("area_no_cycle") => error::invalid("parent_area_id", "would put the area inside itself"),
        _ => error::db(e),
    }
}

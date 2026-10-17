use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

use super::{House, Kind, Links, Location, geojson, json_result, limit, wkb};
use crate::{
    MIB, error,
    files::Sha256Hex,
    view::{self, PdfError, Viewable},
};

/// Text files up to this size come back inline from get_file.
const INLINE_TEXT_MAX: i64 = MIB as i64;
/// Images and PDFs up to this size are shown by get_file.
const VIEW_FILE_MAX: i64 = 64 * MIB as i64;
/// The media types get_file shows as an image.
const IMAGE_TYPES: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FloorParams {
    pub floor_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindParams {
    pub site_id: Option<i64>,
    /// Part of a name, in any case and with or without accents or a leading
    /// article. Matched against `name`, the attributes `common_name`,
    /// `name_en`, `label` and `nickname`, every string in the attribute
    /// `nicknames`, and every outside identifier in `refs`.
    pub name: Option<String>,
    /// A space kind or object kind.
    pub kind: Option<String>,
    /// An outside identifier key, such as `ha_entity_id`, with `ref_value`.
    pub ref_key: Option<String>,
    pub ref_value: Option<String>,
    /// An attribute key; with `attribute_value`, the value must also match.
    pub attribute_key: Option<String>,
    pub attribute_value: Option<Value>,
    /// Default 50, at most 500, for spaces, objects and areas each.
    pub limit: Option<i64>,
    /// Default false: each hit is a short row. True gives the full reply,
    /// adding every attribute, outside identifier and the geometry. This is
    /// about the reply's length, not about detail drawings.
    #[serde(default)]
    pub detail: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WhatIsAtParams {
    pub site_id: i64,
    /// Search outdoor rows and this floor. Omit to search outdoor rows and
    /// every floor of the site.
    pub floor_id: Option<i64>,
    pub x: f64,
    pub y: f64,
    /// Metres, default 1, at most 1000.
    pub radius: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InSpaceParams {
    pub space_id: i64,
    /// Default false: each object is a short row. True gives the full reply,
    /// adding every attribute, outside identifier and the geometry. This is
    /// about the reply's length, not about detail drawings.
    #[serde(default)]
    pub detail: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NearestParams {
    pub site_id: i64,
    /// The object kind to look for, such as `light`.
    pub kind: String,
    pub from: Location,
    /// Only objects on this floor.
    pub floor_id: Option<i64>,
    /// Default 5, at most 100.
    pub n: Option<i64>,
    /// Default false: each hit is a short row. True gives the full reply,
    /// adding every attribute, outside identifier and the geometry. This is
    /// about the reply's length, not about detail drawings.
    #[serde(default)]
    pub detail: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MeasureParams {
    pub a: Location,
    /// When given, the straight line distance from `a` is returned too.
    pub b: Option<Location>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesParams {
    #[serde(flatten)]
    pub links: Links,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFileParams {
    pub sha256: String,
    /// For a PDF, the page to show, from 1. Default 1.
    pub page: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchNotesParams {
    /// Text to look for, any case.
    pub query: String,
    pub site_id: Option<i64>,
    /// Only notes linked to this detail.
    pub detail_id: Option<i64>,
    /// Only notes linked to this area.
    pub area_id: Option<i64>,
    /// Default 20, at most 200.
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryParams {
    pub kind: Kind,
    pub id: i64,
    /// Default 100, at most 1000, newest first.
    pub limit: Option<i64>,
}

/// `%` and `_` in a caller's text match only themselves.
fn like_pattern(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("%{escaped}%")
}

#[tool_router(router = read_router, vis = "pub(super)")]
impl House {
    #[tool(
        description = "List live sites with their attributes, their floors (with each floor's attributes), whether each site is georeferenced, with its origin in WGS 84 and its rotation, and `area`, the area around the site it is directly in ({id, name, kind, via}, or null): `via` is `area_id` when the site names the area, or `origin` when the site names none and its origin falls inside the area's outline. context {\"site_id\": ..} walks on up through the areas around that one."
    )]
    async fn list_sites(&self) -> Result<CallToolResult, ErrorData> {
        let rows = sqlx::query(
            "SELECT s.id, s.name, s.rotation, s.attributes, s.origin IS NOT NULL AS georeferenced,
                    ST_X(ST_Transform(s.origin, 4326)) AS lon, ST_Y(ST_Transform(s.origin, 4326)) AS lat,
                    coalesce((SELECT jsonb_agg(jsonb_build_object('id', f.id, 'name', f.name, 'level', f.level, 'attributes', f.attributes) ORDER BY f.level)
                              FROM live_floor f WHERE f.site_id = s.id), '[]') AS floors,
                    (SELECT jsonb_build_object('id', a.id, 'name', a.name, 'kind', a.kind, 'via', sa.via)
                     FROM house_site_area(s.id) AS sa JOIN live_area a ON a.id = sa.area_id) AS area
             FROM site s WHERE s.deleted_at IS NULL ORDER BY s.name",
        )
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let sites: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "id": r.get::<i64, _>("id"),
                    "name": r.get::<String, _>("name"),
                    "georeferenced": r.get::<bool, _>("georeferenced"),
                    "origin_wgs84": r.get::<Option<f64>, _>("lon").map(|lon| json!({"lon": lon, "lat": r.get::<Option<f64>, _>("lat")})),
                    "rotation": r.get::<f64, _>("rotation"),
                    "attributes": r.get::<Value, _>("attributes"),
                    "floors": r.get::<Value, _>("floors"),
                    "area": r.get::<Option<Value>, _>("area"),
                })
            })
            .collect();
        json_result(sites)
    }

    #[tool(
        description = "A floor with its attributes and every live space and object on it, geometry as GeoJSON in the site frame. Each object carries `placement`: `exact`, `approximate` (near where it is, such as the middle of its room) or `unplaced`, whose geometry is only a stand in."
    )]
    async fn get_floor(&self, Parameters(p): Parameters<FloorParams>) -> Result<CallToolResult, ErrorData> {
        let floor = sqlx::query(
            "SELECT id, site_id, name, level, elevation, ceiling_height, attributes FROM live_floor WHERE id = $1",
        )
        .bind(p.floor_id)
        .fetch_optional(&self.db)
        .await
        .map_err(error::db)?
        .ok_or_else(|| error::invalid("floor_id", "no live floor with this id"))?;
        let spaces = self.spaces_where_floor(p.floor_id).await?;
        let objects = self.objects_where_floor(p.floor_id).await?;
        json_result(json!({
            "id": floor.get::<i64, _>("id"),
            "site_id": floor.get::<i64, _>("site_id"),
            "name": floor.get::<String, _>("name"),
            "level": floor.get::<i32, _>("level"),
            "elevation": floor.get::<f64, _>("elevation"),
            "ceiling_height": floor.get::<Option<f64>, _>("ceiling_height"),
            "attributes": floor.get::<Value, _>("attributes"),
            "spaces": spaces,
            "objects": objects,
        }))
    }

    #[tool(
        description = "Find live spaces and objects by name, kind, outside identifier or attribute. `name` matches part of any name a thing goes by, in any case and with or without accents: the stored `name`, the attributes `common_name`, `name_en`, `label` and `nickname`, every string in the attribute `nicknames`, and every outside identifier in `refs` (such as `ha_name`). A leading `the `, `il ` or `la ` is ignored in the query and in the stored text, so `snug` finds `the snug`. Hits are ranked: an exact match on any field first, then one that starts with the query, then one that contains it, ties by name; `limit` applies after ranking. Each hit says which field matched in `matched_on`, such as `common_name`, `nicknames` or `refs.ha_name`. Every object hit carries `placement`: `exact`, `approximate` (near where it is, such as the middle of its room) or `unplaced` (position not known; find still returns it so it can be placed). Hits are short by default: `id`, `kind`, `name`, `site_id`, `floor_id`, `area` for a space, and for an object `placement` and `in_space`, the smallest live space on its floor that covers its position (null when none does, or when it is unplaced). An object with more inside it says so: `details`, the number of detail drawings of it (see get_detail and context), and `files`, the number of files linked to it, each only when not zero. `detail: true` asks for the full reply (it is about the reply's length, not detail drawings): it adds attributes, outside identifiers and geometry as GeoJSON, and lists an object's detail drawings as `details` [{id, name, view}]. Without `site_id`, find also searches the areas around sites (neighbourhoods, towns, regions) by the same names, and lists any it matches as `areas` [{id, name, kind, parent_area_id}]; `kind` and the attribute filters apply to them too. Example: {\"name\": \"snug\"}."
    )]
    async fn find(&self, Parameters(p): Parameters<FindParams>) -> Result<CallToolResult, ErrorData> {
        let limit = limit("limit", p.limit, 50, 500)?;
        if p.ref_value.is_some() && p.ref_key.is_none() {
            return Err(error::invalid("ref_value", "needs ref_key"));
        }
        if p.attribute_value.is_some() && p.attribute_key.is_none() {
            return Err(error::invalid("attribute_value", "needs attribute_key"));
        }
        let name = p.name.as_deref().map(str::trim);
        if name.is_some_and(str::is_empty) {
            return Err(error::invalid("name", "is empty"));
        }
        let spaces = sqlx::query(concat!(
            "WITH q AS (SELECT ",
            normalised!("$2::text"),
            " AS q)
             SELECT s.id, s.site_id, s.floor_id, s.name, s.kind, s.attributes, ST_Area(s.geom) AS area,
                    ST_AsBinary(s.geom) AS wkb, m.matched_on
             FROM live_space s CROSS JOIN q",
            name_match!(searched_names!("s")),
            "WHERE ($2::text IS NULL OR m.matched_on IS NOT NULL)
               AND ($1::bigint IS NULL OR s.site_id = $1)
               AND ($3::text IS NULL OR s.kind = $3)
               AND $4::text IS NULL
               AND ($5::text IS NULL OR s.attributes ? $5)
               AND ($6::jsonb IS NULL OR s.attributes -> $5 = $6)
             ORDER BY m.rank, s.name, s.id LIMIT $7"
        ))
        .bind(p.site_id)
        .bind(name)
        .bind(&p.kind)
        .bind(&p.ref_key)
        .bind(&p.attribute_key)
        .bind(&p.attribute_value)
        .bind(limit)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let objects = sqlx::query(concat!(
            "SELECT hit.id, hit.site_id, hit.floor_id, hit.name, hit.kind, hit.placement, hit.refs, hit.attributes,
                    ST_AsBinary(hit.geom) AS wkb, hit.matched_on, within.in_space_id, within.in_space_name,
                    levels.detail_count, levels.detail_list, files.file_count
             FROM (
                 WITH q AS (SELECT ",
            normalised!("$2::text"),
            " AS q)
                 SELECT o.*, m.matched_on, m.rank FROM live_object o CROSS JOIN q",
            name_match!(concat!(
                searched_names!("o"),
                " UNION ALL SELECT 'refs.' || r.key, r.value #>> '{}', 6
                    FROM jsonb_each(o.refs) AS r WHERE jsonb_typeof(r.value) = 'string'"
            )),
            "WHERE ($2::text IS NULL OR m.matched_on IS NOT NULL)
                   AND ($1::bigint IS NULL OR o.site_id = $1)
                   AND ($3::text IS NULL OR o.kind = $3)
                   AND ($4::text IS NULL OR o.refs ? $4)
                   AND ($5::text IS NULL OR o.refs ->> $4 = $5)
                   AND ($6::text IS NULL OR o.attributes ? $6)
                   AND ($7::jsonb IS NULL OR o.attributes -> $6 = $7)
                 ORDER BY m.rank, o.name, o.id LIMIT $8
             ) AS hit",
            in_space_join!("hit"),
            object_levels_join!("hit"),
            "ORDER BY hit.rank, hit.name, hit.id"
        ))
        .bind(p.site_id)
        .bind(name)
        .bind(&p.kind)
        .bind(&p.ref_key)
        .bind(&p.ref_value)
        .bind(&p.attribute_key)
        .bind(&p.attribute_value)
        .bind(limit)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let mut space_hits = Vec::with_capacity(spaces.len());
        for r in &spaces {
            let mut hit = json!({
                "id": r.get::<i64, _>("id"),
                "kind": r.get::<String, _>("kind"),
                "name": r.get::<String, _>("name"),
                "site_id": r.get::<i64, _>("site_id"),
                "floor_id": r.get::<Option<i64>, _>("floor_id"),
                "area": r.get::<f64, _>("area"),
            });
            if p.detail {
                hit["attributes"] = r.get::<Value, _>("attributes");
                hit["geometry"] = geojson(&r.get::<Vec<u8>, _>("wkb"))?;
            }
            matched_on(&mut hit, r);
            space_hits.push(hit);
        }
        let mut object_hits = Vec::with_capacity(objects.len());
        for r in &objects {
            let mut hit = object_hit(r, p.detail)?;
            matched_on(&mut hit, r);
            object_hits.push(hit);
        }
        let mut result = json!({ "spaces": space_hits, "objects": object_hits });
        // Areas are not on a site, so a search kept to one site leaves them
        // out, as it does an outside identifier, which no area has.
        if p.site_id.is_none() && p.ref_key.is_none() {
            let areas = sqlx::query(concat!(
                "WITH q AS (SELECT ",
                normalised!("$1::text"),
                " AS q)
                 SELECT a.id, a.name, a.kind, a.parent_area_id, m.matched_on
                 FROM live_area a CROSS JOIN q",
                name_match!(searched_names!("a")),
                "WHERE ($1::text IS NULL OR m.matched_on IS NOT NULL)
                   AND ($2::text IS NULL OR a.kind = $2)
                   AND ($3::text IS NULL OR a.attributes ? $3)
                   AND ($4::jsonb IS NULL OR a.attributes -> $3 = $4)
                 ORDER BY m.rank, a.name, a.id LIMIT $5"
            ))
            .bind(name)
            .bind(&p.kind)
            .bind(&p.attribute_key)
            .bind(&p.attribute_value)
            .bind(limit)
            .fetch_all(&self.db)
            .await
            .map_err(error::db)?;
            if !areas.is_empty() {
                result["areas"] = areas
                    .iter()
                    .map(|r| {
                        let mut hit = json!({
                            "id": r.get::<i64, _>("id"),
                            "name": r.get::<String, _>("name"),
                            "kind": r.get::<String, _>("kind"),
                            "parent_area_id": r.get::<Option<i64>, _>("parent_area_id"),
                        });
                        matched_on(&mut hit, r);
                        hit
                    })
                    .collect();
            }
        }
        json_result(result)
    }

    #[tool(
        description = "What is at a point: the spaces containing it, smallest first, and the objects within a radius (default 1 m), nearest first. With `floor_id`, outdoor rows and that floor are searched. Without it, outdoor rows and every floor of the site are searched, so a point inside the house finds the room under it on each floor. Every hit names its floor (`floor`: id, name and level), or null when it is outdoors. Each object carries `placement`, `exact` or `approximate` (near where it is, such as the middle of its room, so its distance is only as good as that). Objects whose position is not known (`unplaced`) are left out, and `unplaced_omitted` counts those that were within the radius by their stand in position. No geometry is returned."
    )]
    async fn what_is_at(&self, Parameters(p): Parameters<WhatIsAtParams>) -> Result<CallToolResult, ErrorData> {
        let radius = p.radius.unwrap_or(1.0);
        if !(0.0..=1000.0).contains(&radius) {
            return Err(error::invalid("radius", "must be between 0 and 1000 metres"));
        }
        let spaces = sqlx::query(
            "SELECT s.id, s.floor_id, s.name, s.kind, f.name AS floor_name, f.level
             FROM live_space s LEFT JOIN live_floor f ON f.id = s.floor_id
             WHERE s.site_id = $1
               AND (s.floor_id IS NULL OR $2::bigint IS NULL OR s.floor_id = $2)
               AND ST_Covers(s.geom, ST_MakePoint($3, $4))
             ORDER BY f.level NULLS FIRST, ST_Area(s.geom), s.id",
        )
        .bind(p.site_id)
        .bind(p.floor_id)
        .bind(p.x)
        .bind(p.y)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let objects = sqlx::query(
            "SELECT o.id, o.floor_id, o.name, o.kind, o.placement, f.name AS floor_name, f.level,
                    ST_Distance(o.geom, ST_MakePoint($3, $4)) AS distance
             FROM live_object o LEFT JOIN live_floor f ON f.id = o.floor_id
             WHERE o.site_id = $1
               AND (o.floor_id IS NULL OR $2::bigint IS NULL OR o.floor_id = $2)
               AND ST_DWithin(o.geom, ST_MakePoint($3, $4), $5)
             ORDER BY distance, o.id",
        )
        .bind(p.site_id)
        .bind(p.floor_id)
        .bind(p.x)
        .bind(p.y)
        .bind(radius)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let (unplaced, objects): (Vec<_>, Vec<_>) = objects.iter().partition(|r| is_unplaced(r));
        let floor = |r: &sqlx::postgres::PgRow| {
            r.get::<Option<i64>, _>("floor_id").map(|id| {
                json!({ "id": id, "name": r.get::<Option<String>, _>("floor_name"), "level": r.get::<Option<i32>, _>("level") })
            })
        };
        json_result(json!({
            "searched": if p.floor_id.is_some() { "outdoors and the given floor" } else { "outdoors and every floor" },
            "spaces": spaces.iter().map(|r| json!({
                "id": r.get::<i64, _>("id"),
                "floor_id": r.get::<Option<i64>, _>("floor_id"),
                "floor": floor(r),
                "name": r.get::<String, _>("name"),
                "kind": r.get::<String, _>("kind"),
            })).collect::<Vec<_>>(),
            "objects": objects.iter().map(|r| json!({
                "id": r.get::<i64, _>("id"),
                "floor_id": r.get::<Option<i64>, _>("floor_id"),
                "floor": floor(r),
                "name": r.get::<String, _>("name"),
                "kind": r.get::<String, _>("kind"),
                "placement": r.get::<String, _>("placement"),
                "distance": r.get::<Option<f64>, _>("distance"),
            })).collect::<Vec<_>>(),
            "unplaced_omitted": unplaced.len(),
        }))
    }

    #[tool(
        description = "The live objects and smaller spaces inside a space, on the same floor (or outdoors for an outdoor space). Each object carries `placement`, `exact` or `approximate` (near where it is, such as the middle of its room). Objects whose position is not known (`unplaced`) are left out, and `unplaced_omitted` counts those whose stand in position fell inside. Objects are short by default: `id`, `kind`, `name`, `site_id`, `floor_id`, `placement` and `in_space`, the smallest live space covering the object's position, which may be a smaller space inside this one, and `partly_outside: true` on a line or area that crosses the space's edge. An object with more inside it says so: `details`, the number of detail drawings of it (see get_detail and context), and `files`, the number of files linked to it, each only when not zero. `detail: true` asks for the full reply (it is about the reply's length, not detail drawings): it adds attributes, outside identifiers and geometry as GeoJSON, `wholly_inside` on every object, and lists an object's detail drawings as `details` [{id, name, view}]."
    )]
    async fn in_space(&self, Parameters(p): Parameters<InSpaceParams>) -> Result<CallToolResult, ErrorData> {
        let space = sqlx::query("SELECT id FROM live_space WHERE id = $1")
            .bind(p.space_id)
            .fetch_optional(&self.db)
            .await
            .map_err(error::db)?;
        if space.is_none() {
            return Err(error::invalid("space_id", "no live space with this id"));
        }
        let objects = sqlx::query(concat!(
            "SELECT o.id, o.site_id, o.floor_id, o.name, o.kind, o.placement, o.refs, o.attributes,
                    ST_AsBinary(o.geom) AS wkb, ST_CoveredBy(o.geom, s.geom) AS wholly_inside,
                    within.in_space_id, within.in_space_name,
                    levels.detail_count, levels.detail_list, files.file_count
             FROM live_space s JOIN live_object o
               ON o.site_id = s.site_id AND o.floor_id IS NOT DISTINCT FROM s.floor_id AND ST_Intersects(o.geom, s.geom)",
            in_space_join!("o"),
            object_levels_join!("o"),
            "WHERE s.id = $1 ORDER BY o.kind, o.name, o.id"
        ))
        .bind(p.space_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let spaces = sqlx::query(
            "SELECT i.id, i.name, i.kind, ST_Area(i.geom) AS area
             FROM live_space s JOIN live_space i
               ON i.site_id = s.site_id AND i.floor_id IS NOT DISTINCT FROM s.floor_id
              AND i.id <> s.id AND ST_CoveredBy(i.geom, s.geom)
             WHERE s.id = $1 ORDER BY i.name",
        )
        .bind(p.space_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let (unplaced, objects): (Vec<_>, Vec<_>) = objects.iter().partition(|r| is_unplaced(r));
        let mut object_values = Vec::with_capacity(objects.len());
        for row in objects {
            let mut value = object_hit(row, p.detail)?;
            let wholly_inside = row.get::<bool, _>("wholly_inside");
            // A point that meets the space is always inside it, so a short
            // reply names only the exceptions.
            if p.detail {
                value["wholly_inside"] = json!(wholly_inside);
            } else if !wholly_inside {
                value["partly_outside"] = json!(true);
            }
            object_values.push(value);
        }
        json_result(json!({
            "objects": object_values,
            "unplaced_omitted": unplaced.len(),
            "spaces": spaces.iter().map(|r| json!({
                "id": r.get::<i64, _>("id"),
                "name": r.get::<String, _>("name"),
                "kind": r.get::<String, _>("kind"),
                "area": r.get::<f64, _>("area"),
            })).collect::<Vec<_>>(),
        }))
    }

    #[tool(
        description = "The n live objects of a kind nearest to a point, space or object, by straight line distance in plan, nearest first. Returns `objects` and `unplaced_omitted`. Objects whose position is not known (`placement: unplaced`) are never ranked; `unplaced_omitted` counts how many of the kind were left out for that reason. Every hit carries `placement`: an `approximate` one is ranked like any other, but it was placed only near where it is, such as the middle of its room, so a distance to it is only as good as that room. Measuring from an unplaced object is refused. Hits are short by default: `id`, `kind`, `name`, `site_id`, `floor_id`, `placement`, `in_space` (the smallest live space covering the object's position) and `distance`, with `details` (the number of detail drawings of the object) and `files` (the number of files linked to it) when not zero. `detail: true` asks for the full reply (it is about the reply's length, not detail drawings): it adds attributes, outside identifiers and geometry as GeoJSON, and lists the object's detail drawings as `details` [{id, name, view}]. Example: {\"site_id\": 1, \"kind\": \"camera\", \"from\": {\"space\": 7}}."
    )]
    async fn nearest(&self, Parameters(p): Parameters<NearestParams>) -> Result<CallToolResult, ErrorData> {
        let n = limit("n", p.n, 5, 100)?;
        let from = self.location("from", &p.from).await?;
        let exclude = match p.from {
            Location::Object(id) => Some(id),
            _ => None,
        };
        let rows = sqlx::query(concat!(
            "SELECT o.id, o.site_id, o.floor_id, o.name, o.kind, o.placement, o.refs, o.attributes,
                    ST_AsBinary(o.geom) AS wkb, o.distance, within.in_space_id, within.in_space_name,
                    levels.detail_count, levels.detail_list, files.file_count
             FROM (
                 SELECT *, ST_Distance(geom, ST_GeomFromWKB($3, 0)) AS distance
                 FROM live_object
                 WHERE site_id = $1 AND kind = $2 AND placement <> 'unplaced'
                   AND ($4::bigint IS NULL OR floor_id = $4)
                   AND ($5::bigint IS NULL OR id <> $5)
                 ORDER BY geom <-> ST_GeomFromWKB($3, 0), id
                 LIMIT $6
             ) AS o",
            in_space_join!("o"),
            object_levels_join!("o"),
            "ORDER BY o.distance, o.id"
        ))
        .bind(p.site_id)
        .bind(&p.kind)
        .bind(&from)
        .bind(p.floor_id)
        .bind(exclude)
        .bind(n)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let unplaced: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM live_object
             WHERE site_id = $1 AND kind = $2 AND placement = 'unplaced'
               AND ($3::bigint IS NULL OR floor_id = $3)
               AND ($4::bigint IS NULL OR id <> $4)",
        )
        .bind(p.site_id)
        .bind(&p.kind)
        .bind(p.floor_id)
        .bind(exclude)
        .fetch_one(&self.db)
        .await
        .map_err(error::db)?;
        let mut found = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut value = object_hit(row, p.detail)?;
            value["distance"] = json!(row.get::<Option<f64>, _>("distance"));
            found.push(value);
        }
        json_result(json!({ "objects": found, "unplaced_omitted": unplaced }))
    }

    #[tool(
        description = "Measure: area and perimeter of a space or polygon, length of a line, and with `b` the straight line distance between the two. An unplaced object, whose position is not known, cannot be measured."
    )]
    async fn measure(&self, Parameters(p): Parameters<MeasureParams>) -> Result<CallToolResult, ErrorData> {
        let a = self.location("a", &p.a).await?;
        let mut result = json!({ "a": self.shape_measures(&a).await? });
        if let Some(b) = &p.b {
            let b = self.location("b", b).await?;
            result["b"] = self.shape_measures(&b).await?;
            let distance: Option<f64> =
                sqlx::query_scalar("SELECT ST_Distance(ST_GeomFromWKB($1, 0), ST_GeomFromWKB($2, 0))")
                    .bind(&a)
                    .bind(&b)
                    .fetch_one(&self.db)
                    .await
                    .map_err(error::db)?;
            result["distance"] = json!(distance);
        }
        json_result(result)
    }

    #[tool(
        description = "List live files, optionally only those linked to a site, floor, space or object, to a detail (`detail_id`, a drawing inside an object) or to an area around sites (`area_id`, such as a town). get_file shows one: text inline, an image as an image, a PDF page by page."
    )]
    async fn list_files(&self, Parameters(p): Parameters<ListFilesParams>) -> Result<CallToolResult, ErrorData> {
        let rows = sqlx::query(
            "SELECT f.id, f.name, f.media_type, f.size, f.sha256, f.site_id, f.floor_id, f.space_id, f.object_id,
                    k.detail_id, k.area_id, f.created_at
             FROM file f LEFT JOIN file_link k ON k.file_id = f.id
             WHERE f.deleted_at IS NULL
               AND ($1::bigint IS NULL OR f.site_id = $1)
               AND ($2::bigint IS NULL OR f.floor_id = $2)
               AND ($3::bigint IS NULL OR f.space_id = $3)
               AND ($4::bigint IS NULL OR f.object_id = $4)
               AND ($5::bigint IS NULL OR k.detail_id = $5)
               AND ($6::bigint IS NULL OR k.area_id = $6)
             ORDER BY f.created_at DESC, f.id DESC",
        )
        .bind(p.links.site_id)
        .bind(p.links.floor_id)
        .bind(p.links.space_id)
        .bind(p.links.object_id)
        .bind(p.links.detail_id)
        .bind(p.links.area_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        json_result(rows.iter().map(file_json).collect::<Vec<_>>())
    }

    #[tool(
        description = "A file's records by sha256, and its content in the form that suits it; `content_kind` says which. `text`: UTF-8 text of at most 1 MiB, in `content`. `image`: a PNG, JPEG or WebP, returned as an image no larger than 2000 px on its long side (scaled down when larger; `width` and `height` are what is returned, `original_width` and `original_height` the stored image). `pdf_page`: one page of a PDF, `page` (from 1, default 1), drawn to a PNG about 2000 px on its long side, with `page_count`. `none`: any other file, or one that cannot be read, with `content` null. Example: {\"sha256\": \"...\", \"page\": 2}."
    )]
    async fn get_file(&self, Parameters(p): Parameters<GetFileParams>) -> Result<CallToolResult, ErrorData> {
        let sha = Sha256Hex::parse(&p.sha256).ok_or_else(|| error::invalid("sha256", "not a lowercase hex sha256"))?;
        let rows = sqlx::query(
            "SELECT f.id, f.name, f.media_type, f.size, f.sha256, f.site_id, f.floor_id, f.space_id, f.object_id,
                    k.detail_id, k.area_id, f.created_at
             FROM file f LEFT JOIN file_link k ON k.file_id = f.id
             WHERE f.sha256 = $1 AND f.deleted_at IS NULL ORDER BY f.id",
        )
        .bind(sha.as_str())
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let Some(first) = rows.first() else {
            return Err(error::invalid("sha256", "no live file with this hash"));
        };
        let media_type: String = first.get("media_type");
        let size: i64 = first.get("size");
        let mut reply = json!({ "records": rows.iter().map(file_json).collect::<Vec<_>>(), "content": null });
        let is_pdf = media_type == "application/pdf";
        if p.page.is_some() && !is_pdf {
            return Err(error::invalid("page", "only a PDF has pages"));
        }
        let shown = if is_pdf || IMAGE_TYPES.contains(&media_type.as_str()) {
            if size > VIEW_FILE_MAX {
                reply["content_note"] = json!(format!(
                    "larger than {} MiB, too large to show",
                    VIEW_FILE_MAX / MIB as i64
                ));
                None
            } else {
                let bytes = self
                    .files
                    .get(&sha)
                    .await
                    .map_err(|e| error::internal("reading a file", e))?;
                if is_pdf {
                    self.pdf_page(bytes, p.page.unwrap_or(1), &mut reply).await?
                } else {
                    let shown = tokio::task::spawn_blocking(move || view::image(&bytes, &media_type))
                        .await
                        .map_err(|e| error::internal("reading an image", e))?;
                    match shown {
                        Ok((viewable, (width, height))) => {
                            reply["content_kind"] = json!("image");
                            reply["width"] = json!(viewable.width);
                            reply["height"] = json!(viewable.height);
                            reply["original_width"] = json!(width);
                            reply["original_height"] = json!(height);
                            Some(viewable)
                        }
                        Err(e) => {
                            reply["content_note"] = json!(e);
                            None
                        }
                    }
                }
            }
        } else {
            if size <= INLINE_TEXT_MAX {
                let bytes = self
                    .files
                    .get(&sha)
                    .await
                    .map_err(|e| error::internal("reading a file", e))?;
                if let Some(text) = String::from_utf8(bytes).ok().filter(|text| !text.contains('\0')) {
                    reply["content_kind"] = json!("text");
                    reply["content"] = json!(text);
                }
            }
            None
        };
        if reply.get("content_kind").is_none() {
            reply["content_kind"] = json!("none");
        }
        let text = serde_json::to_string(&reply).map_err(|e| error::internal("encoding a result", e))?;
        let mut content = Vec::with_capacity(2);
        if let Some(viewable) = shown {
            content.push(ContentBlock::image(
                STANDARD.encode(&viewable.bytes),
                viewable.media_type,
            ));
        }
        content.push(ContentBlock::text(text));
        Ok(CallToolResult::success(content))
    }

    #[tool(
        description = "Search live notes for text, newest first, optionally only those on a site (`site_id`), linked to a detail (`detail_id`, a drawing inside an object) or linked to an area around sites (`area_id`)."
    )]
    async fn search_notes(&self, Parameters(p): Parameters<SearchNotesParams>) -> Result<CallToolResult, ErrorData> {
        let limit = limit("limit", p.limit, 20, 200)?;
        if p.query.trim().is_empty() {
            return Err(error::invalid("query", "is empty"));
        }
        let rows = sqlx::query(
            "SELECT n.id, n.body, n.source, n.site_id, n.floor_id, n.space_id, n.object_id, k.detail_id, k.area_id, n.created_at
             FROM note n LEFT JOIN note_link k ON k.note_id = n.id
             WHERE n.deleted_at IS NULL AND n.body ILIKE $1 AND ($2::bigint IS NULL OR n.site_id = $2)
               AND ($4::bigint IS NULL OR k.detail_id = $4) AND ($5::bigint IS NULL OR k.area_id = $5)
             ORDER BY n.created_at DESC, n.id DESC LIMIT $3",
        )
        .bind(like_pattern(p.query.trim()))
        .bind(p.site_id)
        .bind(limit)
        .bind(p.detail_id)
        .bind(p.area_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        json_result(
            rows.iter()
                .map(|r| {
                    json!({
                        "id": r.get::<i64, _>("id"),
                        "body": r.get::<String, _>("body"),
                        "source": r.get::<String, _>("source"),
                        "site_id": r.get::<Option<i64>, _>("site_id"),
                        "floor_id": r.get::<Option<i64>, _>("floor_id"),
                        "space_id": r.get::<Option<i64>, _>("space_id"),
                        "object_id": r.get::<Option<i64>, _>("object_id"),
                        "detail_id": r.get::<Option<i64>, _>("detail_id"),
                        "area_id": r.get::<Option<i64>, _>("area_id"),
                        "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
                    })
                })
                .collect::<Vec<_>>(),
        )
    }

    #[tool(
        description = "Every recorded change to one thing, from any client, newest first, with the row before and after."
    )]
    async fn history(&self, Parameters(p): Parameters<HistoryParams>) -> Result<CallToolResult, ErrorData> {
        let limit = limit("limit", p.limit, 100, 1000)?;
        let rows = sqlx::query(
            "SELECT id, at, author, client, reason, operation, before, after FROM change
             WHERE table_name = $1 AND row_id = $2 ORDER BY id DESC LIMIT $3",
        )
        .bind(p.kind.table())
        .bind(p.id)
        .bind(limit)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        json_result(
            rows.iter()
                .map(|r| {
                    json!({
                        "change_id": r.get::<i64, _>("id"),
                        "at": r.get::<chrono::DateTime<chrono::Utc>, _>("at"),
                        "author": r.get::<String, _>("author"),
                        "client": r.get::<String, _>("client"),
                        "reason": r.get::<String, _>("reason"),
                        "operation": r.get::<String, _>("operation"),
                        "before": r.get::<Option<Value>, _>("before"),
                        "after": r.get::<Option<Value>, _>("after"),
                    })
                })
                .collect::<Vec<_>>(),
        )
    }
}

impl House {
    /// One page of a stored PDF to show, describing it in `reply`. A page
    /// outside the document is the caller's mistake; an unreadable document
    /// is shown as nothing, with a note.
    async fn pdf_page(&self, bytes: Vec<u8>, page: usize, reply: &mut Value) -> Result<Option<Viewable>, ErrorData> {
        if page == 0 {
            return Err(error::invalid("page", "pages are numbered from 1"));
        }
        let shown = tokio::task::spawn_blocking(move || view::pdf_page(bytes, page))
            .await
            .map_err(|e| error::internal("drawing a PDF page", e))?;
        match shown {
            Ok((viewable, count)) => {
                reply["content_kind"] = json!("pdf_page");
                reply["page"] = json!(page);
                reply["page_count"] = json!(count);
                reply["width"] = json!(viewable.width);
                reply["height"] = json!(viewable.height);
                Ok(Some(viewable))
            }
            Err(PdfError::NoSuchPage(count)) => Err(error::invalid(
                "page",
                format!("the document has {count} page{}", if count == 1 { "" } else { "s" }),
            )),
            Err(PdfError::Unreadable(e)) => {
                reply["content_note"] = json!(e);
                Ok(None)
            }
        }
    }

    /// A location's geometry as WKB in the site frame.
    async fn location(&self, field: &str, location: &Location) -> Result<Vec<u8>, ErrorData> {
        let (sql, id) = match location {
            Location::Point([x, y]) => return wkb(field, &json!({"type": "Point", "coordinates": [x, y]})),
            Location::Space(id) => (
                "SELECT ST_AsBinary(geom), 'exact'::text FROM live_space WHERE id = $1",
                id,
            ),
            Location::Object(id) => ("SELECT ST_AsBinary(geom), placement FROM live_object WHERE id = $1", id),
        };
        let (geometry, placement): (Vec<u8>, String) = sqlx::query_as(sql)
            .bind(id)
            .fetch_optional(&self.db)
            .await
            .map_err(error::db)?
            .ok_or_else(|| error::invalid(field, "names no live space or object"))?;
        if placement == "unplaced" {
            return Err(error::invalid(
                field,
                "names an unplaced object, whose position is not known",
            ));
        }
        Ok(geometry)
    }

    async fn shape_measures(&self, wkb: &[u8]) -> Result<Value, ErrorData> {
        let row = sqlx::query(
            "SELECT GeometryType(g) AS kind, ST_Area(g) AS area, ST_Perimeter(g) AS perimeter, ST_Length(g) AS length
             FROM ST_GeomFromWKB($1, 0) AS g",
        )
        .bind(wkb)
        .fetch_one(&self.db)
        .await
        .map_err(error::db)?;
        Ok(json!({
            "geometry_type": row.get::<String, _>("kind"),
            "area": row.get::<f64, _>("area"),
            "perimeter": row.get::<f64, _>("perimeter"),
            "length": row.get::<f64, _>("length"),
        }))
    }

    async fn spaces_where_floor(&self, floor_id: i64) -> Result<Vec<Value>, ErrorData> {
        let rows = sqlx::query(
            "SELECT id, name, kind, wall_material, attributes, ST_Area(geom) AS area, ST_AsBinary(geom) AS wkb
             FROM live_space WHERE floor_id = $1 ORDER BY name",
        )
        .bind(floor_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        rows.iter()
            .map(|r| {
                Ok(json!({
                    "id": r.get::<i64, _>("id"),
                    "name": r.get::<String, _>("name"),
                    "kind": r.get::<String, _>("kind"),
                    "wall_material": r.get::<Option<String>, _>("wall_material"),
                    "attributes": r.get::<Value, _>("attributes"),
                    "area": r.get::<f64, _>("area"),
                    "geometry": geojson(&r.get::<Vec<u8>, _>("wkb"))?,
                }))
            })
            .collect()
    }

    async fn objects_where_floor(&self, floor_id: i64) -> Result<Vec<Value>, ErrorData> {
        let rows = sqlx::query(
            "SELECT id, site_id, floor_id, name, kind, placement, refs, attributes, ST_AsBinary(geom) AS wkb
             FROM live_object WHERE floor_id = $1 ORDER BY kind, name",
        )
        .bind(floor_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        rows.iter().map(object_json).collect()
    }
}

fn object_json(r: &sqlx::postgres::PgRow) -> Result<Value, ErrorData> {
    Ok(json!({
        "id": r.get::<i64, _>("id"),
        "site_id": r.get::<i64, _>("site_id"),
        "floor_id": r.get::<Option<i64>, _>("floor_id"),
        "name": r.get::<String, _>("name"),
        "kind": r.get::<String, _>("kind"),
        "placement": r.get::<String, _>("placement"),
        "refs": r.get::<Value, _>("refs"),
        "attributes": r.get::<Value, _>("attributes"),
        "geometry": geojson(&r.get::<Vec<u8>, _>("wkb"))?,
    }))
}

/// An object as a hit from find, in_space or nearest: short, or with
/// `detail` everything object_json gives. Either way it names the smallest
/// space covering its position, from `in_space_join!`, and its detail
/// drawings and files when it has any: counted in a short hit, the details
/// listed in a full one.
fn object_hit(r: &sqlx::postgres::PgRow, detail: bool) -> Result<Value, ErrorData> {
    let mut hit = if detail {
        object_json(r)?
    } else {
        json!({
            "id": r.get::<i64, _>("id"),
            "kind": r.get::<String, _>("kind"),
            "name": r.get::<String, _>("name"),
            "site_id": r.get::<i64, _>("site_id"),
            "floor_id": r.get::<Option<i64>, _>("floor_id"),
            "placement": r.get::<String, _>("placement"),
        })
    };
    hit["in_space"] = match r.get::<Option<i64>, _>("in_space_id") {
        Some(id) => json!({ "id": id, "name": r.get::<Option<String>, _>("in_space_name") }),
        None => Value::Null,
    };
    // What more there is inside the object, from `object_levels_join!`, named
    // only when there is some.
    let details: i64 = r.get("detail_count");
    if details > 0 {
        hit["details"] = if detail {
            r.get::<Value, _>("detail_list")
        } else {
            json!(details)
        };
    }
    let files: i64 = r.get("file_count");
    if files > 0 {
        hit["files"] = json!(files);
    }
    Ok(hit)
}

/// Adds the field a name search matched on, when the search had a name.
fn matched_on(hit: &mut Value, r: &sqlx::postgres::PgRow) {
    if let Some(field) = r.get::<Option<String>, _>("matched_on") {
        hit["matched_on"] = json!(field);
    }
}

fn is_unplaced(r: &sqlx::postgres::PgRow) -> bool {
    r.get::<&str, _>("placement") == "unplaced"
}

fn file_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": r.get::<i64, _>("id"),
        "name": r.get::<String, _>("name"),
        "media_type": r.get::<String, _>("media_type"),
        "size": r.get::<i64, _>("size"),
        "sha256": r.get::<String, _>("sha256"),
        "site_id": r.get::<Option<i64>, _>("site_id"),
        "floor_id": r.get::<Option<i64>, _>("floor_id"),
        "space_id": r.get::<Option<i64>, _>("space_id"),
        "object_id": r.get::<Option<i64>, _>("object_id"),
        "detail_id": r.get::<Option<i64>, _>("detail_id"),
        "area_id": r.get::<Option<i64>, _>("area_id"),
        "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
    })
}

#[cfg(test)]
mod tests {
    use super::like_pattern;

    #[test]
    fn like_wildcards_in_text_are_escaped() {
        assert_eq!(like_pattern("50%_off"), r"%50\%\_off%");
    }
}

use rmcp::{ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

use super::{House, Kind, Links, Location, geojson, json_result, limit, wkb};
use crate::{MIB, error, files::Sha256Hex};

/// Text files up to this size come back inline from get_file.
const INLINE_TEXT_MAX: i64 = MIB as i64;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FloorParams {
    pub floor_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindParams {
    pub site_id: Option<i64>,
    /// Part of a name, any case.
    pub name: Option<String>,
    /// A space kind or object kind.
    pub kind: Option<String>,
    /// An outside identifier key, such as `ha_entity_id`, with `ref_value`.
    pub ref_key: Option<String>,
    pub ref_value: Option<String>,
    /// An attribute key; with `attribute_value`, the value must also match.
    pub attribute_key: Option<String>,
    pub attribute_value: Option<Value>,
    /// Default 50, at most 500, for spaces and objects each.
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WhatIsAtParams {
    pub site_id: i64,
    /// Without a floor, only outdoor spaces and objects are considered.
    pub floor_id: Option<i64>,
    pub x: f64,
    pub y: f64,
    /// Metres, default 1, at most 1000.
    pub radius: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpaceParams {
    pub space_id: i64,
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
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchNotesParams {
    /// Text to look for, any case.
    pub query: String,
    pub site_id: Option<i64>,
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
    #[tool(description = "List live sites with their floors and whether each site is georeferenced.")]
    async fn list_sites(&self) -> Result<CallToolResult, ErrorData> {
        let rows = sqlx::query(
            "SELECT s.id, s.name, s.rotation, s.origin IS NOT NULL AS georeferenced,
                    ST_X(ST_Transform(s.origin, 4326)) AS lon, ST_Y(ST_Transform(s.origin, 4326)) AS lat,
                    coalesce((SELECT jsonb_agg(jsonb_build_object('id', f.id, 'name', f.name, 'level', f.level) ORDER BY f.level)
                              FROM floor f WHERE f.site_id = s.id AND f.deleted_at IS NULL), '[]') AS floors
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
                    "floors": r.get::<Value, _>("floors"),
                })
            })
            .collect();
        json_result(sites)
    }

    #[tool(description = "A floor with every live space and object on it, geometry as GeoJSON in the site frame.")]
    async fn get_floor(&self, Parameters(p): Parameters<FloorParams>) -> Result<CallToolResult, ErrorData> {
        let floor = sqlx::query(
            "SELECT id, site_id, name, level, elevation, ceiling_height FROM floor WHERE id = $1 AND deleted_at IS NULL",
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
            "spaces": spaces,
            "objects": objects,
        }))
    }

    #[tool(description = "Find live spaces and objects by part of a name, kind, outside identifier or attribute.")]
    async fn find(&self, Parameters(p): Parameters<FindParams>) -> Result<CallToolResult, ErrorData> {
        let limit = limit("limit", p.limit, 50, 500)?;
        if p.ref_value.is_some() && p.ref_key.is_none() {
            return Err(error::invalid("ref_value", "needs ref_key"));
        }
        if p.attribute_value.is_some() && p.attribute_key.is_none() {
            return Err(error::invalid("attribute_value", "needs attribute_key"));
        }
        let name = p.name.as_deref().map(like_pattern);
        let spaces = sqlx::query(
            "SELECT id, site_id, floor_id, name, kind, attributes, ST_Area(geom) AS area FROM space
             WHERE deleted_at IS NULL
               AND ($1::bigint IS NULL OR site_id = $1)
               AND ($2::text IS NULL OR name ILIKE $2)
               AND ($3::text IS NULL OR kind = $3)
               AND $4::text IS NULL
               AND ($5::text IS NULL OR attributes ? $5)
               AND ($6::jsonb IS NULL OR attributes -> $5 = $6)
             ORDER BY name LIMIT $7",
        )
        .bind(p.site_id)
        .bind(&name)
        .bind(&p.kind)
        .bind(&p.ref_key)
        .bind(&p.attribute_key)
        .bind(&p.attribute_value)
        .bind(limit)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let objects = sqlx::query(
            "SELECT id, site_id, floor_id, name, kind, refs, attributes, ST_AsBinary(geom) AS wkb FROM object
             WHERE deleted_at IS NULL
               AND ($1::bigint IS NULL OR site_id = $1)
               AND ($2::text IS NULL OR name ILIKE $2)
               AND ($3::text IS NULL OR kind = $3)
               AND ($4::text IS NULL OR refs ? $4)
               AND ($5::text IS NULL OR refs ->> $4 = $5)
               AND ($6::text IS NULL OR attributes ? $6)
               AND ($7::jsonb IS NULL OR attributes -> $6 = $7)
             ORDER BY name LIMIT $8",
        )
        .bind(p.site_id)
        .bind(&name)
        .bind(&p.kind)
        .bind(&p.ref_key)
        .bind(&p.ref_value)
        .bind(&p.attribute_key)
        .bind(&p.attribute_value)
        .bind(limit)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        json_result(json!({
            "spaces": spaces.iter().map(|r| json!({
                "id": r.get::<i64, _>("id"),
                "site_id": r.get::<i64, _>("site_id"),
                "floor_id": r.get::<Option<i64>, _>("floor_id"),
                "name": r.get::<String, _>("name"),
                "kind": r.get::<String, _>("kind"),
                "area": r.get::<f64, _>("area"),
                "attributes": r.get::<Value, _>("attributes"),
            })).collect::<Vec<_>>(),
            "objects": objects.iter().map(object_json).collect::<Result<Vec<_>, _>>()?,
        }))
    }

    #[tool(
        description = "What is at a point: the spaces containing it, and the objects within a radius, nearest first."
    )]
    async fn what_is_at(&self, Parameters(p): Parameters<WhatIsAtParams>) -> Result<CallToolResult, ErrorData> {
        let radius = p.radius.unwrap_or(1.0);
        if !(0.0..=1000.0).contains(&radius) {
            return Err(error::invalid("radius", "must be between 0 and 1000 metres"));
        }
        let spaces = sqlx::query(
            "SELECT id, floor_id, name, kind FROM space
             WHERE site_id = $1 AND deleted_at IS NULL
               AND (floor_id IS NULL OR floor_id = $2)
               AND ST_Covers(geom, ST_MakePoint($3, $4))
             ORDER BY ST_Area(geom)",
        )
        .bind(p.site_id)
        .bind(p.floor_id)
        .bind(p.x)
        .bind(p.y)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let objects = sqlx::query(
            "SELECT id, floor_id, name, kind, ST_Distance(geom, ST_MakePoint($3, $4)) AS distance FROM object
             WHERE site_id = $1 AND deleted_at IS NULL
               AND (floor_id IS NULL OR floor_id = $2)
               AND ST_DWithin(geom, ST_MakePoint($3, $4), $5)
             ORDER BY distance, id",
        )
        .bind(p.site_id)
        .bind(p.floor_id)
        .bind(p.x)
        .bind(p.y)
        .bind(radius)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        json_result(json!({
            "spaces": spaces.iter().map(|r| json!({
                "id": r.get::<i64, _>("id"),
                "floor_id": r.get::<Option<i64>, _>("floor_id"),
                "name": r.get::<String, _>("name"),
                "kind": r.get::<String, _>("kind"),
            })).collect::<Vec<_>>(),
            "objects": objects.iter().map(|r| json!({
                "id": r.get::<i64, _>("id"),
                "floor_id": r.get::<Option<i64>, _>("floor_id"),
                "name": r.get::<String, _>("name"),
                "kind": r.get::<String, _>("kind"),
                "distance": r.get::<Option<f64>, _>("distance"),
            })).collect::<Vec<_>>(),
        }))
    }

    #[tool(
        description = "The live objects and smaller spaces inside a space, on the same floor (or outdoors for an outdoor space)."
    )]
    async fn in_space(&self, Parameters(p): Parameters<SpaceParams>) -> Result<CallToolResult, ErrorData> {
        let space = sqlx::query("SELECT id FROM space WHERE id = $1 AND deleted_at IS NULL")
            .bind(p.space_id)
            .fetch_optional(&self.db)
            .await
            .map_err(error::db)?;
        if space.is_none() {
            return Err(error::invalid("space_id", "no live space with this id"));
        }
        let objects = sqlx::query(
            "SELECT o.id, o.site_id, o.floor_id, o.name, o.kind, o.refs, o.attributes, ST_AsBinary(o.geom) AS wkb,
                    ST_CoveredBy(o.geom, s.geom) AS wholly_inside
             FROM space s JOIN object o
               ON o.site_id = s.site_id AND o.floor_id IS NOT DISTINCT FROM s.floor_id AND ST_Intersects(o.geom, s.geom)
             WHERE s.id = $1 AND o.deleted_at IS NULL ORDER BY o.kind, o.name",
        )
        .bind(p.space_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let spaces = sqlx::query(
            "SELECT i.id, i.name, i.kind, ST_Area(i.geom) AS area
             FROM space s JOIN space i
               ON i.site_id = s.site_id AND i.floor_id IS NOT DISTINCT FROM s.floor_id
              AND i.id <> s.id AND ST_CoveredBy(i.geom, s.geom)
             WHERE s.id = $1 AND i.deleted_at IS NULL ORDER BY i.name",
        )
        .bind(p.space_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let mut object_values = Vec::with_capacity(objects.len());
        for row in &objects {
            let mut value = object_json(row)?;
            value["wholly_inside"] = json!(row.get::<bool, _>("wholly_inside"));
            object_values.push(value);
        }
        json_result(json!({
            "objects": object_values,
            "spaces": spaces.iter().map(|r| json!({
                "id": r.get::<i64, _>("id"),
                "name": r.get::<String, _>("name"),
                "kind": r.get::<String, _>("kind"),
                "area": r.get::<f64, _>("area"),
            })).collect::<Vec<_>>(),
        }))
    }

    #[tool(
        description = "The n live objects of a kind nearest to a point, space or object, by straight line distance in plan."
    )]
    async fn nearest(&self, Parameters(p): Parameters<NearestParams>) -> Result<CallToolResult, ErrorData> {
        let n = limit("n", p.n, 5, 100)?;
        let from = self.location("from", &p.from).await?;
        let exclude = match p.from {
            Location::Object(id) => Some(id),
            _ => None,
        };
        let rows = sqlx::query(
            "SELECT id, site_id, floor_id, name, kind, refs, attributes, ST_AsBinary(geom) AS wkb,
                    ST_Distance(geom, ST_GeomFromWKB($3, 0)) AS distance
             FROM object
             WHERE site_id = $1 AND kind = $2 AND deleted_at IS NULL
               AND ($4::bigint IS NULL OR floor_id = $4)
               AND ($5::bigint IS NULL OR id <> $5)
             ORDER BY geom <-> ST_GeomFromWKB($3, 0), id
             LIMIT $6",
        )
        .bind(p.site_id)
        .bind(&p.kind)
        .bind(&from)
        .bind(p.floor_id)
        .bind(exclude)
        .bind(n)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let mut found = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut value = object_json(row)?;
            value["distance"] = json!(row.get::<Option<f64>, _>("distance"));
            found.push(value);
        }
        json_result(found)
    }

    #[tool(
        description = "Measure: area and perimeter of a space or polygon, length of a line, and with `b` the straight line distance between the two."
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

    #[tool(description = "List live files, optionally only those linked to a site, floor, space or object.")]
    async fn list_files(&self, Parameters(p): Parameters<ListFilesParams>) -> Result<CallToolResult, ErrorData> {
        let rows = sqlx::query(
            "SELECT id, name, media_type, size, sha256, site_id, floor_id, space_id, object_id, created_at FROM file
             WHERE deleted_at IS NULL
               AND ($1::bigint IS NULL OR site_id = $1)
               AND ($2::bigint IS NULL OR floor_id = $2)
               AND ($3::bigint IS NULL OR space_id = $3)
               AND ($4::bigint IS NULL OR object_id = $4)
             ORDER BY created_at DESC, id DESC",
        )
        .bind(p.links.site_id)
        .bind(p.links.floor_id)
        .bind(p.links.space_id)
        .bind(p.links.object_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        json_result(rows.iter().map(file_json).collect::<Vec<_>>())
    }

    #[tool(description = "A file's metadata by sha256, with its content when it is UTF-8 text of at most 1 MiB.")]
    async fn get_file(&self, Parameters(p): Parameters<GetFileParams>) -> Result<CallToolResult, ErrorData> {
        let sha = Sha256Hex::parse(&p.sha256).ok_or_else(|| error::invalid("sha256", "not a lowercase hex sha256"))?;
        let rows = sqlx::query(
            "SELECT id, name, media_type, size, sha256, site_id, floor_id, space_id, object_id, created_at FROM file
             WHERE sha256 = $1 AND deleted_at IS NULL ORDER BY id",
        )
        .bind(sha.as_str())
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let Some(first) = rows.first() else {
            return Err(error::invalid("sha256", "no live file with this hash"));
        };
        let content = if first.get::<i64, _>("size") <= INLINE_TEXT_MAX {
            let bytes = self
                .files
                .get(&sha)
                .await
                .map_err(|e| error::internal("reading a file", e))?;
            String::from_utf8(bytes).ok().filter(|text| !text.contains('\0'))
        } else {
            None
        };
        json_result(json!({
            "records": rows.iter().map(file_json).collect::<Vec<_>>(),
            "content": content,
        }))
    }

    #[tool(description = "Search live notes for text, newest first.")]
    async fn search_notes(&self, Parameters(p): Parameters<SearchNotesParams>) -> Result<CallToolResult, ErrorData> {
        let limit = limit("limit", p.limit, 20, 200)?;
        if p.query.trim().is_empty() {
            return Err(error::invalid("query", "is empty"));
        }
        let rows = sqlx::query(
            "SELECT id, body, source, site_id, floor_id, space_id, object_id, created_at FROM note
             WHERE deleted_at IS NULL AND body ILIKE $1 AND ($2::bigint IS NULL OR site_id = $2)
             ORDER BY created_at DESC, id DESC LIMIT $3",
        )
        .bind(like_pattern(p.query.trim()))
        .bind(p.site_id)
        .bind(limit)
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
    /// A location's geometry as WKB in the site frame.
    async fn location(&self, field: &str, location: &Location) -> Result<Vec<u8>, ErrorData> {
        let (sql, id) = match location {
            Location::Point([x, y]) => return wkb(field, &json!({"type": "Point", "coordinates": [x, y]})),
            Location::Space(id) => (
                "SELECT ST_AsBinary(geom) FROM space WHERE id = $1 AND deleted_at IS NULL",
                id,
            ),
            Location::Object(id) => (
                "SELECT ST_AsBinary(geom) FROM object WHERE id = $1 AND deleted_at IS NULL",
                id,
            ),
        };
        sqlx::query_scalar(sql)
            .bind(id)
            .fetch_optional(&self.db)
            .await
            .map_err(error::db)?
            .ok_or_else(|| error::invalid(field, "names no live space or object"))
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
             FROM space WHERE floor_id = $1 AND deleted_at IS NULL ORDER BY name",
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
            "SELECT id, site_id, floor_id, name, kind, refs, attributes, ST_AsBinary(geom) AS wkb
             FROM object WHERE floor_id = $1 AND deleted_at IS NULL ORDER BY kind, name",
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
        "refs": r.get::<Value, _>("refs"),
        "attributes": r.get::<Value, _>("attributes"),
        "geometry": geojson(&r.get::<Vec<u8>, _>("wkb"))?,
    }))
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

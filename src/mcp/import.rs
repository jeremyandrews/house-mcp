use std::collections::HashMap;

use axum::http::request::Parts;
use rmcp::{
    ErrorData,
    handler::server::{tool::Extension, wrapper::Parameters},
    model::CallToolResult,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};

use super::{
    House, json_result, reason, wkb,
    write::{OBJECT_GEOMETRY_TYPES, check_epsg, check_geometry, check_space_kind, object_kind},
    writer,
};
use crate::{
    db,
    dxf_import::{self, LayerTarget, Parsed, Transform},
    error,
    units::LengthUnit,
};

const MAX_FEATURES: usize = 10_000;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FeatureTarget {
    Space,
    Object,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImportGeoJsonParams {
    pub site_id: i64,
    /// Put everything on this floor; omit for outdoor spaces and objects.
    pub floor_id: Option<i64>,
    /// A stored GeoJSON file. Give this or `geojson`.
    pub file_sha256: Option<String>,
    /// An inline FeatureCollection or Feature.
    pub geojson: Option<Value>,
    /// EPSG code of the coordinates. Default 4326, longitude and latitude.
    pub epsg: Option<i32>,
    /// Whether each feature becomes a space or an object.
    #[serde(rename = "as")]
    pub target: FeatureTarget,
    /// The kind for every feature, unless `kind_property` names a property
    /// that holds it.
    pub kind: Option<String>,
    pub kind_property: Option<String>,
    /// The property holding each feature's name. Features without one are
    /// named by their position in the file.
    pub name_property: Option<String>,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImportDxfParams {
    /// The sha256 of a stored DXF file.
    pub file_sha256: String,
    /// The floor to import onto. Omit, and give `site_id`, to import a
    /// drawing of the grounds onto the site itself.
    pub floor_id: Option<i64>,
    /// Needed without `floor_id`. With it, it may be left out, and if given
    /// must be the floor's site.
    pub site_id: Option<i64>,
    /// DXF layer name (any case) to what its entities become beyond linework,
    /// such as `{"ROOMS": {"space": "room"}, "LIGHTS": {"object": "light"}}`.
    #[serde(default)]
    pub layers: HashMap<String, LayerTarget>,
    /// The drawing's unit, needed when its $INSUNITS header names none, and
    /// used instead of the header when given.
    pub unit: Option<LengthUnit>,
    #[serde(default)]
    pub transform: Transform,
    pub reason: String,
}

#[tool_router(router = import_router, vis = "pub(super)")]
impl House {
    #[tool(
        description = "Import GeoJSON features in any EPSG coordinate system as spaces or objects, converted into the site frame. This is how a land registry parcel or a geoportal layer arrives. The site must be georeferenced."
    )]
    async fn import_geojson(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(mut p): Parameters<ImportGeoJsonParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        let document = match (&p.file_sha256, p.geojson.take()) {
            (Some(sha), None) => self.stored_json(sha).await?,
            (None, Some(inline)) => inline,
            _ => return Err(error::invalid("geojson", "give exactly one of file_sha256 and geojson")),
        };
        let features = features(&document)?;
        let mut rows = Vec::with_capacity(features.len());
        for (i, feature) in features.iter().enumerate() {
            rows.push(feature_row(&p, i, feature)?);
        }
        let epsg = p.epsg.unwrap_or(4326);
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        check_epsg(&mut tx, "epsg", epsg).await?;
        let mut created = Vec::with_capacity(rows.len());
        for row in &rows {
            let made = insert_feature(&mut tx, &p, epsg, row).await?;
            created.extend(
                made.into_iter()
                    .map(|(id, name)| json!({ "id": id, "name": name, "kind": row.kind })),
            );
        }
        tx.commit().await.map_err(error::db)?;
        json_result(json!({ "as": p.target, "created": created }))
    }

    #[tool(
        description = "Import a stored DXF drawing onto a floor, or with `site_id` and no `floor_id` onto the site itself, as for a drawing of the grounds. Every entity is kept as linework, which render_plan draws on that floor, or on the site's outdoor plan (no `floor_id`) for a site import. Closed polylines on a layer mapped to a space kind become spaces named by the text inside them, and block insertions on a layer mapped to an object kind become objects; a site import makes them outdoor ones. A second import of the same file onto the same floor, or onto the same site with no floor, replaces the first. DWG is not read."
    )]
    async fn import_dxf(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ImportDxfParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        let mut layers = HashMap::with_capacity(p.layers.len());
        for (layer, target) in &p.layers {
            let field = format!("layers.{layer}");
            let target = match target {
                LayerTarget::Space(kind) => {
                    check_space_kind(&field, kind)?;
                    LayerTarget::Space(kind.clone())
                }
                LayerTarget::Object(kind) => LayerTarget::Object(object_kind(&field, kind)?),
            };
            layers.insert(layer.clone(), target);
        }
        if !(p.transform.scale.is_finite() && p.transform.scale > 0.0) {
            return Err(error::invalid("transform.scale", "must be positive"));
        }
        let (file, bytes) = self.import_bytes("file_sha256", &p.file_sha256).await?;
        if is_dwg(&file.name, &bytes) {
            return Err(error::invalid(
                "file_sha256",
                "is a DWG drawing, which this server does not read; convert it to DXF with the free ODA File Converter, upload the DXF and import that",
            ));
        }
        let options = dxf_import::Options {
            unit: p.unit,
            layers: layers.clone(),
            transform: p.transform.clone(),
        };
        let parsed = tokio::task::spawn_blocking(move || dxf_import::parse(&bytes, &options))
            .await
            .map_err(|e| error::internal("reading a DXF file", e))?
            .map_err(|e| error::invalid("file_sha256", e))?;

        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        let site_id = import_site(&mut tx, p.floor_id, p.site_id).await?;
        let replaced = retire_previous_import(&mut tx, file.sha.as_str(), site_id, p.floor_id).await?;
        let summary = json!({
            "unit": parsed.unit,
            "unit_from": if parsed.unit_from_call { "call" } else { "$INSUNITS" },
            "layers": parsed.layers,
            "skipped": skipped_json(&parsed),
        });
        let options = json!({ "layers": layers, "unit": p.unit, "transform": p.transform });
        let import_id: i64 = sqlx::query_scalar(
            "INSERT INTO dxf_import (file_id, sha256, floor_id, site_id, unit, options, summary)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(file.id)
        .bind(file.sha.as_str())
        .bind(p.floor_id)
        .bind(site_id)
        .bind(parsed.unit.name())
        .bind(&options)
        .bind(&summary)
        .fetch_one(&mut *tx)
        .await
        .map_err(error::db)?;
        insert_linework(&mut tx, import_id, site_id, p.floor_id, &parsed).await?;
        let (spaces, invalid) = insert_spaces(&mut tx, import_id, site_id, p.floor_id, &parsed).await?;
        let objects = insert_objects(&mut tx, import_id, site_id, p.floor_id, &parsed).await?;
        tx.commit().await.map_err(error::db)?;
        let mut result = summary;
        result["import_id"] = json!(import_id);
        result["replaced_import_id"] = json!(replaced);
        result["linework"] = json!(parsed.linework.len());
        result["spaces"] = json!(spaces);
        result["objects"] = json!(objects);
        result["invalid_outlines"] = json!(invalid);
        json_result(result)
    }
}

impl House {
    async fn stored_json(&self, sha: &str) -> Result<Value, ErrorData> {
        let (_, bytes) = self.import_bytes("file_sha256", sha).await?;
        serde_json::from_slice(&bytes).map_err(|e| error::invalid("file_sha256", format!("not JSON: {e}")))
    }
}

/// The features of a FeatureCollection, or a single Feature.
fn features(document: &Value) -> Result<Vec<&Value>, ErrorData> {
    let features: Vec<&Value> = match document.get("type").and_then(Value::as_str) {
        Some("FeatureCollection") => document
            .get("features")
            .and_then(Value::as_array)
            .ok_or_else(|| error::invalid("geojson", "FeatureCollection has no features array"))?
            .iter()
            .collect(),
        Some("Feature") => vec![document],
        _ => return Err(error::invalid("geojson", "expected a FeatureCollection or a Feature")),
    };
    if features.len() > MAX_FEATURES {
        return Err(error::invalid(
            "geojson",
            format!("has more than {MAX_FEATURES} features"),
        ));
    }
    Ok(features)
}

struct FeatureRow {
    /// `features[i]`, so an error names the feature at fault.
    field: String,
    name: String,
    kind: String,
    wkb: Vec<u8>,
}

fn feature_row(p: &ImportGeoJsonParams, i: usize, feature: &Value) -> Result<FeatureRow, ErrorData> {
    let field = format!("features[{i}]");
    let property = |key: &Option<String>| {
        key.as_ref()
            .and_then(|key| feature.get("properties")?.get(key))
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
    };
    let kind = property(&p.kind_property)
        .or_else(|| p.kind.clone())
        .ok_or_else(|| error::invalid(&field, "has no kind; give `kind` or a `kind_property` it has"))?;
    let kind = match p.target {
        FeatureTarget::Space => {
            check_space_kind(&field, &kind)?;
            kind
        }
        FeatureTarget::Object => object_kind(&field, &kind)?,
    };
    let name = match property(&p.name_property) {
        Some(name) => feature_name(&field, &name)?,
        None => format!("{kind} {}", i + 1),
    };
    let geometry = feature
        .get("geometry")
        .ok_or_else(|| error::invalid(&field, "has no geometry"))?;
    let wkb = wkb(&field, geometry)?;
    Ok(FeatureRow { field, name, kind, wkb })
}

/// One feature into the site frame and the model. A space takes each polygon
/// of a multipolygon as its own space, its name numbered by part. Returns
/// the id and the stored name of each row made.
async fn insert_feature(
    tx: &mut PgConnection,
    p: &ImportGeoJsonParams,
    epsg: i32,
    row: &FeatureRow,
) -> Result<Vec<(i64, String)>, ErrorData> {
    let local: Vec<u8> =
        sqlx::query_scalar("SELECT ST_AsBinary(house_to_local(ST_SetSRID(ST_GeomFromWKB($1), $2), $3))")
            .bind(&row.wkb)
            .bind(epsg)
            .bind(p.site_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| conversion_error(e, &row.field, epsg))?;
    let allowed = match p.target {
        FeatureTarget::Space => &["POLYGON", "MULTIPOLYGON"][..],
        FeatureTarget::Object => OBJECT_GEOMETRY_TYPES,
    };
    check_geometry(tx, &row.field, &local, allowed).await?;
    let sql = match p.target {
        FeatureTarget::Space => {
            "INSERT INTO space (site_id, floor_id, name, kind, geom)
             SELECT $2, $3, CASE WHEN count(*) OVER () > 1 THEN left($4 || ' (' || d.path[1] || ')', 200) ELSE $4 END, $5, d.geom
             FROM ST_Dump(ST_GeomFromWKB($1, 0)) AS d
             RETURNING id, name"
        }
        FeatureTarget::Object => {
            "INSERT INTO object (site_id, floor_id, name, kind, geom)
             VALUES ($2, $3, $4, $5, ST_GeomFromWKB($1, 0))
             RETURNING id, name"
        }
    };
    sqlx::query_as(sql)
        .bind(&local)
        .bind(p.site_id)
        .bind(p.floor_id)
        .bind(&row.name)
        .bind(&row.kind)
        .fetch_all(&mut *tx)
        .await
        .map_err(error::db)
}

/// The site an import goes onto: the floor's when there is a floor, which
/// `site_id` must then agree with, or else `site_id`.
async fn import_site(tx: &mut PgConnection, floor_id: Option<i64>, site_id: Option<i64>) -> Result<i64, ErrorData> {
    match (floor_id, site_id) {
        (Some(floor_id), site_id) => {
            let floor_site: i64 = sqlx::query_scalar("SELECT site_id FROM live_floor WHERE id = $1")
                .bind(floor_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(error::db)?
                .ok_or_else(|| error::invalid("floor_id", "no live floor with this id"))?;
            if site_id.is_some_and(|site| site != floor_site) {
                return Err(error::invalid("site_id", "is not the floor's site"));
            }
            Ok(floor_site)
        }
        (None, Some(site_id)) => {
            let live: Option<i64> = sqlx::query_scalar("SELECT id FROM site WHERE id = $1 AND deleted_at IS NULL")
                .bind(site_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(error::db)?;
            live.ok_or_else(|| error::invalid("site_id", "no live site with this id"))
        }
        (None, None) => Err(error::invalid(
            "floor_id",
            "give floor_id to import onto a floor, or site_id alone to import onto the site",
        )),
    }
}

/// Marks an earlier import of the same content onto the same floor, or onto
/// the same site with no floor, as deleted, with everything it created, and
/// returns its id.
async fn retire_previous_import(
    tx: &mut PgConnection,
    sha256: &str,
    site_id: i64,
    floor_id: Option<i64>,
) -> Result<Option<i64>, ErrorData> {
    let previous: Option<i64> = sqlx::query_scalar(
        "UPDATE dxf_import SET deleted_at = now()
         WHERE sha256 = $1 AND deleted_at IS NULL
           AND CASE WHEN $2::bigint IS NULL THEN floor_id IS NULL AND site_id = $3 ELSE floor_id = $2 END
         RETURNING id",
    )
    .bind(sha256)
    .bind(floor_id)
    .bind(site_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(error::db)?;
    if let Some(id) = previous {
        for sql in [
            "UPDATE linework SET deleted_at = now() WHERE dxf_import_id = $1 AND deleted_at IS NULL",
            "UPDATE space SET deleted_at = now() WHERE dxf_import_id = $1 AND deleted_at IS NULL",
            "UPDATE object SET deleted_at = now() WHERE dxf_import_id = $1 AND deleted_at IS NULL",
        ] {
            sqlx::query(sql).bind(id).execute(&mut *tx).await.map_err(error::db)?;
        }
    }
    Ok(previous)
}

async fn insert_linework(
    tx: &mut PgConnection,
    import_id: i64,
    site_id: i64,
    floor_id: Option<i64>,
    parsed: &Parsed,
) -> Result<(), ErrorData> {
    let mut layers = Vec::new();
    let mut entities = Vec::new();
    let mut geometries = Vec::new();
    let mut texts = Vec::new();
    let mut heights = Vec::new();
    for item in &parsed.linework {
        layers.push(item.layer.as_str());
        entities.push(item.entity);
        geometries.push(crate::geom::to_wkb(&item.geometry).map_err(|e| error::internal("encoding linework", e))?);
        texts.push(item.text.as_deref());
        heights.push(item.text_height);
    }
    sqlx::query(
        "INSERT INTO linework (dxf_import_id, floor_id, site_id, layer, entity, geom, text, text_height)
         SELECT $1, $2, $8, u.layer, u.entity, ST_GeomFromWKB(u.wkb, 0), u.text, u.height
         FROM unnest($3::text[], $4::text[], $5::bytea[], $6::text[], $7::float8[]) AS u(layer, entity, wkb, text, height)",
    )
    .bind(import_id)
    .bind(floor_id)
    .bind(&layers)
    .bind(&entities)
    .bind(&geometries)
    .bind(&texts)
    .bind(&heights)
    .bind(site_id)
    .execute(&mut *tx)
    .await
    .map_err(error::db)?;
    Ok(())
}

/// Closed outlines on space layers into spaces. Each takes its name from the
/// tallest text of this import that PostGIS finds inside it, or from its layer
/// when there is none. Outlines PostGIS finds invalid are reported, not
/// stored.
async fn insert_spaces(
    tx: &mut PgConnection,
    import_id: i64,
    site_id: i64,
    floor_id: Option<i64>,
    parsed: &Parsed,
) -> Result<(Vec<Value>, Vec<Value>), ErrorData> {
    let mut geometries = Vec::new();
    let mut kinds = Vec::new();
    let mut layers = Vec::new();
    for candidate in &parsed.spaces {
        geometries
            .push(crate::geom::to_wkb(&candidate.polygon).map_err(|e| error::internal("encoding an outline", e))?);
        kinds.push(candidate.kind.as_str());
        layers.push(candidate.layer.as_str());
    }
    let rows = sqlx::query(
        "WITH candidate AS (
             SELECT ST_GeomFromWKB(u.wkb, 0) AS geom, u.kind, u.layer, u.n
             FROM unnest($4::bytea[], $5::text[], $6::text[]) WITH ORDINALITY AS u(wkb, kind, layer, n)
         ), inserted AS (
             INSERT INTO space (site_id, floor_id, name, kind, geom, dxf_import_id)
             SELECT $2, $3, coalesce(left(btrim(label.text), 200), left(c.layer || ' ' || c.n, 200)), c.kind, c.geom, $1
             FROM candidate c
             LEFT JOIN LATERAL (
                 SELECT l.text FROM linework l
                 WHERE l.dxf_import_id = $1 AND l.deleted_at IS NULL AND btrim(l.text) <> '' AND ST_Contains(c.geom, l.geom)
                 ORDER BY l.text_height DESC NULLS LAST, l.id LIMIT 1
             ) AS label ON true
             WHERE ST_IsValid(c.geom)
             RETURNING id, name, kind
         )
         SELECT id, name, kind, NULL AS layer, NULL AS problem FROM inserted
         UNION ALL
         SELECT NULL, NULL, kind, layer, ST_IsValidReason(geom) FROM candidate WHERE NOT ST_IsValid(geom)",
    )
    .bind(import_id)
    .bind(site_id)
    .bind(floor_id)
    .bind(&geometries)
    .bind(&kinds)
    .bind(&layers)
    .fetch_all(&mut *tx)
    .await
    .map_err(error::db)?;
    let mut spaces = Vec::new();
    let mut invalid = Vec::new();
    for row in rows {
        match row.get::<Option<i64>, _>("id") {
            Some(id) => spaces
                .push(json!({ "id": id, "name": row.get::<String, _>("name"), "kind": row.get::<String, _>("kind") })),
            None => invalid.push(json!({
                "layer": row.get::<Option<String>, _>("layer"),
                "problem": row.get::<Option<String>, _>("problem"),
            })),
        }
    }
    Ok((spaces, invalid))
}

/// Block insertions on object layers into objects, named by their block and
/// numbered when a block appears more than once.
async fn insert_objects(
    tx: &mut PgConnection,
    import_id: i64,
    site_id: i64,
    floor_id: Option<i64>,
    parsed: &Parsed,
) -> Result<Vec<Value>, ErrorData> {
    let mut geometries = Vec::new();
    let mut kinds = Vec::new();
    let mut blocks = Vec::new();
    let mut layers = Vec::new();
    for candidate in &parsed.objects {
        geometries.push(crate::geom::to_wkb(&candidate.point).map_err(|e| error::internal("encoding a point", e))?);
        kinds.push(candidate.kind.as_str());
        blocks.push(candidate.block.as_str());
        layers.push(candidate.layer.as_str());
    }
    let rows = sqlx::query(
        "INSERT INTO object (site_id, floor_id, kind, name, geom, attributes, dxf_import_id)
         SELECT $2, $3, u.kind,
                CASE WHEN count(*) OVER (PARTITION BY u.block) > 1
                     THEN left(u.block || ' ' || row_number() OVER (PARTITION BY u.block ORDER BY u.n), 200)
                     ELSE left(u.block, 200) END,
                ST_GeomFromWKB(u.wkb, 0), jsonb_build_object('dxf_block', u.block, 'dxf_layer', u.layer), $1
         FROM unnest($4::bytea[], $5::text[], $6::text[], $7::text[]) WITH ORDINALITY AS u(wkb, kind, block, layer, n)
         RETURNING id, name, kind",
    )
    .bind(import_id)
    .bind(site_id)
    .bind(floor_id)
    .bind(&geometries)
    .bind(&kinds)
    .bind(&blocks)
    .bind(&layers)
    .fetch_all(&mut *tx)
    .await
    .map_err(error::db)?;
    Ok(rows
        .iter()
        .map(|r| json!({ "id": r.get::<i64, _>("id"), "name": r.get::<String, _>("name"), "kind": r.get::<String, _>("kind") }))
        .collect())
}

fn skipped_json(parsed: &Parsed) -> Vec<Value> {
    parsed
        .skipped
        .iter()
        .map(|(s, count)| json!({ "layer": s.layer, "entity": s.entity, "reason": s.reason, "count": count }))
        .collect()
}

/// By name or by the "AC10" version string every DWG file starts with.
fn is_dwg(name: &str, bytes: &[u8]) -> bool {
    name.to_ascii_lowercase().ends_with(".dwg") || bytes.starts_with(b"AC10")
}

/// A feature's own name, trimmed. Refused rather than cut, because the caller
/// chose it and a silently shortened name would not be the one they look for.
fn feature_name(field: &str, name: &str) -> Result<String, ErrorData> {
    let name = name.trim();
    match name.chars().count() {
        0 => Err(error::invalid(field, "has a blank name")),
        1..=200 => Ok(name.to_owned()),
        _ => Err(error::invalid(field, "has a name longer than 200 characters")),
    }
}

/// A feature whose coordinates PostGIS cannot carry into the site frame is the
/// caller's mistake; a site without a georeference keeps its own message.
fn conversion_error(error: sqlx::Error, field: &str, epsg: i32) -> ErrorData {
    let georeference = error
        .as_database_error()
        .and_then(|e| e.code())
        .is_some_and(|code| code == "22023");
    match &error {
        sqlx::Error::Database(_) if !georeference => {
            error::invalid(field, format!("coordinates cannot be converted from EPSG:{epsg}"))
        }
        _ => error::db(error),
    }
}

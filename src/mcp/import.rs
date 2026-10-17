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
    House, json_result,
    levels::check_live_detail,
    reason, wkb,
    write::{OBJECT_GEOMETRY_TYPES, check_epsg, check_geometry, check_live_area, check_space_kind, object_kind},
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
    /// An area around sites, such as a town from a geoportal; needs no site.
    Area,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImportGeoJsonParams {
    /// The site to import spaces or objects onto. Not used for areas.
    pub site_id: Option<i64>,
    /// Put everything on this floor; omit for outdoor spaces and objects.
    pub floor_id: Option<i64>,
    /// For areas: the area every imported area is inside.
    pub parent_area_id: Option<i64>,
    /// A stored GeoJSON file. Give this or `geojson`.
    pub file_sha256: Option<String>,
    /// An inline FeatureCollection or Feature.
    pub geojson: Option<Value>,
    /// EPSG code of the coordinates. Default 4326, longitude and latitude.
    pub epsg: Option<i32>,
    /// Whether each feature becomes a space, an object or an area.
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
    /// A detail to import onto instead (a drawing of an object, from
    /// upsert_detail), given alone. The drawing is converted to the detail's
    /// millimetres, and layers mapped to `part` become its parts.
    pub detail_id: Option<i64>,
    /// Needed without `floor_id`. With it, it may be left out, and if given
    /// must be the floor's site.
    pub site_id: Option<i64>,
    /// DXF layer name (any case) to what its entities become beyond linework,
    /// such as `{"ROOMS": {"space": "room"}, "LIGHTS": {"object": "light"}}`
    /// on a floor or site, or `{"PARTS": {"part": "part"}}` on a detail.
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
        description = "Import GeoJSON features in any EPSG coordinate system as spaces or objects, converted into the site frame, or as areas around sites (`as: area`), stored in WGS 84. This is how a land registry parcel, a geoportal layer or a town's boundary arrives. Spaces and objects need `site_id`, and the site must be georeferenced. Areas need no site: each feature becomes one area, of `kind` (such as `town`) or the kind in `kind_property`, inside `parent_area_id` when given, and its geometry must be a Polygon or MultiPolygon. Example: {\"as\": \"area\", \"kind\": \"municipality\", \"name_property\": \"NAME\", \"file_sha256\": \"...\", \"reason\": \"boundary from the geoportal\"}."
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
        match p.target {
            FeatureTarget::Area if p.site_id.is_some() || p.floor_id.is_some() => {
                return Err(error::invalid(
                    "site_id",
                    "an area is not on a site; leave out site_id and floor_id",
                ));
            }
            FeatureTarget::Space | FeatureTarget::Object if p.site_id.is_none() => {
                return Err(error::invalid("site_id", "is required to import spaces or objects"));
            }
            FeatureTarget::Space | FeatureTarget::Object if p.parent_area_id.is_some() => {
                return Err(error::invalid("parent_area_id", "is only for `as: area`"));
            }
            _ => {}
        }
        let mut rows = Vec::with_capacity(features.len());
        for (i, feature) in features.iter().enumerate() {
            rows.push(feature_row(&p, i, feature)?);
        }
        let epsg = p.epsg.unwrap_or(4326);
        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        check_epsg(&mut tx, "epsg", epsg).await?;
        if let Some(parent) = p.parent_area_id {
            check_live_area(&mut tx, "parent_area_id", parent).await?;
        }
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
        description = "Import a stored DXF drawing onto a floor, or with `site_id` and no `floor_id` onto the site itself, as for a drawing of the grounds, or with `detail_id` alone onto a detail, as for a maker's drawing of a piece of furniture. Every entity is kept as linework, which render_plan draws on that floor, on the site's outdoor plan (no `floor_id`) for a site import, or on the detail (render_plan {\"detail_id\": ..}) for a detail import, never on a floor or site plan. On a floor or site, closed polylines on a layer mapped to a space kind become spaces named by the text inside them, and block insertions on a layer mapped to an object kind become objects; a site import makes them outdoor ones. On a detail the drawing is converted to the detail's frame, millimetres, from its unit (metres, inches or any other), and closed polylines on a layer mapped to `part` become the detail's labelled parts, named by the text inside them or by the mapped word and a number: {\"detail_id\": 4, \"layers\": {\"PARTS\": {\"part\": \"panel\"}}}. A second import of the same file onto the same floor, the same site with no floor, or the same detail replaces the first. DWG is not read."
    )]
    async fn import_dxf(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ImportDxfParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let author = &writer(&parts)?.name;
        let reason = reason(&p.reason)?;
        if let Some(detail) = p.detail_id {
            return self.import_dxf_onto_detail(author, reason, detail, &p).await;
        }
        let mut layers = HashMap::with_capacity(p.layers.len());
        for (layer, target) in &p.layers {
            let field = format!("layers.{layer}");
            let target = match target {
                LayerTarget::Space(kind) => {
                    check_space_kind(&field, kind)?;
                    LayerTarget::Space(kind.clone())
                }
                LayerTarget::Object(kind) => LayerTarget::Object(object_kind(&field, kind)?),
                LayerTarget::Part(_) => {
                    return Err(error::invalid(&field, "part layers are for a detail; give detail_id"));
                }
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
            frame_unit: LengthUnit::Metres,
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
    /// import_dxf onto a detail: linework into detail_linework, outlines on
    /// part layers into parts, all in the detail's millimetres.
    async fn import_dxf_onto_detail(
        &self,
        author: &str,
        reason: &str,
        detail_id: i64,
        p: &ImportDxfParams,
    ) -> Result<CallToolResult, ErrorData> {
        if p.floor_id.is_some() || p.site_id.is_some() {
            return Err(error::invalid(
                "detail_id",
                "give detail_id alone; a detail is not on a floor, and its site is its object's",
            ));
        }
        let mut layers = HashMap::with_capacity(p.layers.len());
        for (layer, target) in &p.layers {
            let field = format!("layers.{layer}");
            match target {
                LayerTarget::Part(word) => {
                    let word = word.trim();
                    if word.is_empty() || word.chars().count() > 100 {
                        return Err(error::invalid(&field, "the part word must be 1 to 100 characters"));
                    }
                    layers.insert(layer.clone(), LayerTarget::Part(word.to_owned()));
                }
                _ => {
                    return Err(error::invalid(
                        &field,
                        "a detail takes only part layers, such as {\"part\": \"panel\"}",
                    ));
                }
            }
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
            frame_unit: LengthUnit::Millimetres,
        };
        let parsed = tokio::task::spawn_blocking(move || dxf_import::parse(&bytes, &options))
            .await
            .map_err(|e| error::internal("reading a DXF file", e))?
            .map_err(|e| error::invalid("file_sha256", e))?;

        let mut tx = db::begin_write(&self.db, author, reason).await.map_err(error::db)?;
        let site_id = check_live_detail(&mut tx, "detail_id", detail_id).await?;
        let replaced = retire_previous_detail_import(&mut tx, file.sha.as_str(), detail_id).await?;
        let summary = json!({
            "unit": parsed.unit,
            "unit_from": if parsed.unit_from_call { "call" } else { "$INSUNITS" },
            "frame": "millimetres",
            "layers": parsed.layers,
            "skipped": skipped_json(&parsed),
        });
        let options = json!({ "layers": layers, "unit": p.unit, "transform": p.transform });
        let import_id: i64 = sqlx::query_scalar(
            "INSERT INTO dxf_import (file_id, sha256, detail_id, unit, options, summary)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(file.id)
        .bind(file.sha.as_str())
        .bind(detail_id)
        .bind(parsed.unit.name())
        .bind(&options)
        .bind(&summary)
        .fetch_one(&mut *tx)
        .await
        .map_err(error::db)?;
        insert_detail_linework(&mut tx, import_id, detail_id, site_id, &parsed).await?;
        let (parts, invalid) = insert_parts(&mut tx, import_id, detail_id, &parsed).await?;
        tx.commit().await.map_err(error::db)?;
        let mut result = summary;
        result["import_id"] = json!(import_id);
        result["replaced_import_id"] = json!(replaced);
        result["linework"] = json!(parsed.linework.len());
        result["parts"] = json!(parts);
        result["invalid_outlines"] = json!(invalid);
        json_result(result)
    }

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
        FeatureTarget::Object | FeatureTarget::Area => object_kind(&field, &kind)?,
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
    if let FeatureTarget::Area = p.target {
        return insert_area(tx, p, epsg, row).await;
    }
    let local: Vec<u8> =
        sqlx::query_scalar("SELECT ST_AsBinary(house_to_local(ST_SetSRID(ST_GeomFromWKB($1), $2), $3))")
            .bind(&row.wkb)
            .bind(epsg)
            .bind(p.site_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| conversion_error(e, &row.field, epsg))?;
    let allowed = match p.target {
        FeatureTarget::Space | FeatureTarget::Area => &["POLYGON", "MULTIPOLYGON"][..],
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
        FeatureTarget::Object | FeatureTarget::Area => {
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

/// One feature as an area: its outline in WGS 84, whole, inside the import's
/// `parent_area_id` when it has one.
async fn insert_area(
    tx: &mut PgConnection,
    p: &ImportGeoJsonParams,
    epsg: i32,
    row: &FeatureRow,
) -> Result<Vec<(i64, String)>, ErrorData> {
    check_geometry(tx, &row.field, &row.wkb, &["POLYGON", "MULTIPOLYGON"]).await?;
    sqlx::query_as(
        "INSERT INTO area (name, kind, parent_area_id, geom)
         VALUES ($3, $4, $5, ST_Transform(ST_SetSRID(ST_GeomFromWKB($1), $2), 4326))
         RETURNING id, name",
    )
    .bind(&row.wkb)
    .bind(epsg)
    .bind(&row.name)
    .bind(&row.kind)
    .bind(p.parent_area_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| conversion_error(e, &row.field, epsg))
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

/// Marks an earlier import of the same content onto the same detail as
/// deleted, with its linework and parts, and returns its id.
async fn retire_previous_detail_import(
    tx: &mut PgConnection,
    sha256: &str,
    detail_id: i64,
) -> Result<Option<i64>, ErrorData> {
    let previous: Option<i64> = sqlx::query_scalar(
        "UPDATE dxf_import SET deleted_at = now()
         WHERE sha256 = $1 AND deleted_at IS NULL AND detail_id = $2
         RETURNING id",
    )
    .bind(sha256)
    .bind(detail_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(error::db)?;
    if let Some(id) = previous {
        for sql in [
            "UPDATE detail_linework SET deleted_at = now() WHERE dxf_import_id = $1 AND deleted_at IS NULL",
            "UPDATE detail_part SET deleted_at = now() WHERE dxf_import_id = $1 AND deleted_at IS NULL",
        ] {
            sqlx::query(sql).bind(id).execute(&mut *tx).await.map_err(error::db)?;
        }
    }
    Ok(previous)
}

/// A drawing's linework as the arrays the insert statements unnest.
type LineworkArrays<'p> = (
    Vec<&'p str>,
    Vec<&'static str>,
    Vec<Vec<u8>>,
    Vec<Option<&'p str>>,
    Vec<Option<f64>>,
);

fn linework_arrays(parsed: &Parsed) -> Result<LineworkArrays<'_>, ErrorData> {
    let mut arrays: LineworkArrays<'_> = Default::default();
    for item in &parsed.linework {
        arrays.0.push(item.layer.as_str());
        arrays.1.push(item.entity);
        arrays
            .2
            .push(crate::geom::to_wkb(&item.geometry).map_err(|e| error::internal("encoding linework", e))?);
        arrays.3.push(item.text.as_deref());
        arrays.4.push(item.text_height);
    }
    Ok(arrays)
}

async fn insert_detail_linework(
    tx: &mut PgConnection,
    import_id: i64,
    detail_id: i64,
    site_id: i64,
    parsed: &Parsed,
) -> Result<(), ErrorData> {
    let (layers, entities, geometries, texts, heights) = linework_arrays(parsed)?;
    sqlx::query(
        "INSERT INTO detail_linework (dxf_import_id, detail_id, site_id, layer, entity, geom, text, text_height)
         SELECT $1, $2, $8, u.layer, u.entity, ST_GeomFromWKB(u.wkb, 0), u.text, u.height
         FROM unnest($3::text[], $4::text[], $5::bytea[], $6::text[], $7::float8[]) AS u(layer, entity, wkb, text, height)",
    )
    .bind(import_id)
    .bind(detail_id)
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

/// Closed outlines on part layers into the detail's parts, each named by the
/// tallest text of this import inside it, or by its layer's word and a
/// number. Outlines PostGIS finds invalid are reported, not stored.
async fn insert_parts(
    tx: &mut PgConnection,
    import_id: i64,
    detail_id: i64,
    parsed: &Parsed,
) -> Result<(Vec<Value>, Vec<Value>), ErrorData> {
    let mut geometries = Vec::new();
    let mut words = Vec::new();
    let mut layers = Vec::new();
    for candidate in &parsed.parts {
        geometries
            .push(crate::geom::to_wkb(&candidate.polygon).map_err(|e| error::internal("encoding an outline", e))?);
        words.push(candidate.kind.as_str());
        layers.push(candidate.layer.as_str());
    }
    let rows = sqlx::query(
        "WITH candidate AS (
             SELECT ST_GeomFromWKB(u.wkb, 0) AS geom, u.word, u.layer, u.n
             FROM unnest($3::bytea[], $4::text[], $5::text[]) WITH ORDINALITY AS u(wkb, word, layer, n)
         ), labelled AS (
             SELECT c.*, btrim(label.text) AS text
             FROM candidate c
             LEFT JOIN LATERAL (
                 SELECT l.text FROM detail_linework l
                 WHERE l.dxf_import_id = $1 AND l.deleted_at IS NULL AND btrim(l.text) <> '' AND ST_Contains(c.geom, l.geom)
                 ORDER BY l.text_height DESC NULLS LAST, l.id LIMIT 1
             ) AS label ON true
             WHERE ST_IsValid(c.geom)
         ), inserted AS (
             -- The outlines with no text are numbered among themselves.
             INSERT INTO detail_part (detail_id, name, geom, attributes, dxf_import_id)
             SELECT $2,
                    coalesce(left(l.text, 200),
                             left(l.word || ' ' || row_number() OVER (PARTITION BY l.word, l.text IS NULL ORDER BY l.n), 200)),
                    l.geom, jsonb_build_object('dxf_layer', l.layer), $1
             FROM labelled l
             ORDER BY l.n
             RETURNING id, name
         )
         SELECT id, name, NULL AS layer, NULL AS problem FROM inserted
         UNION ALL
         SELECT NULL, NULL, layer, ST_IsValidReason(geom) FROM candidate WHERE NOT ST_IsValid(geom)",
    )
    .bind(import_id)
    .bind(detail_id)
    .bind(&geometries)
    .bind(&words)
    .bind(&layers)
    .fetch_all(&mut *tx)
    .await
    .map_err(error::db)?;
    let mut parts = Vec::new();
    let mut invalid = Vec::new();
    for row in rows {
        match row.get::<Option<i64>, _>("id") {
            Some(id) => parts.push(json!({ "id": id, "name": row.get::<String, _>("name") })),
            None => invalid.push(json!({
                "layer": row.get::<Option<String>, _>("layer"),
                "problem": row.get::<Option<String>, _>("problem"),
            })),
        }
    }
    Ok((parts, invalid))
}

async fn insert_linework(
    tx: &mut PgConnection,
    import_id: i64,
    site_id: i64,
    floor_id: Option<i64>,
    parsed: &Parsed,
) -> Result<(), ErrorData> {
    let (layers, entities, geometries, texts, heights) = linework_arrays(parsed)?;
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

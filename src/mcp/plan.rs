use std::collections::HashSet;

use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use sqlx::{Row, postgres::PgRow};

use super::House;
use crate::{
    error,
    render::{self, Extent, Feature, Label, MAX_SIDE_PX, MIN_SIDE_PX, Plan, Style},
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenderPlanParams {
    pub site_id: i64,
    /// Omit to draw the site: outdoor spaces and objects.
    pub floor_id: Option<i64>,
    #[serde(default)]
    pub layers: Layers,
    /// Spaces and objects to draw in a highlight colour, such as
    /// `[{"object": 12}, {"space": 3}]`.
    #[serde(default)]
    pub highlight: Vec<Highlight>,
    /// Pixels on the long side. Default 1600, from 200 to 4096.
    pub size: Option<u32>,
    /// Attribute keys tried in order for each label; the first whose value
    /// is a non-empty string wins, otherwise the row's `name`. Omitted means
    /// `["common_name", "name_en"]`. An empty list means always `name`.
    pub label_from: Option<Vec<String>>,
}

/// The label keys when `label_from` is omitted.
const DEFAULT_LABEL_FROM: [&str; 2] = ["common_name", "name_en"];

/// An opening's wall depth in metres when it has no valid `wall_depth`.
const DEFAULT_WALL_DEPTH: f64 = 0.6;

/// An `opening` object that cuts the floor's linework.
struct Opening {
    id: i64,
    wkb: Vec<u8>,
    from: (f64, f64),
    to: (f64, f64),
    wall_depth: f64,
    arch: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Layers {
    /// Imported CAD linework. Default true.
    #[serde(default = "yes")]
    pub linework: bool,
    /// Default true.
    #[serde(default = "yes")]
    pub spaces: bool,
    /// Names of spaces and objects. Default true.
    #[serde(default = "yes")]
    pub labels: bool,
    /// Object kinds to draw. Omit for every kind; an empty list draws none.
    pub objects: Option<Vec<String>>,
}

fn yes() -> bool {
    true
}

impl Default for Layers {
    fn default() -> Self {
        Self {
            linework: true,
            spaces: true,
            labels: true,
            objects: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Highlight {
    Space(i64),
    Object(i64),
}

#[tool_router(router = plan_router, vis = "pub(super)")]
impl House {
    #[tool(
        description = "Draw a floor, or the site's outdoor areas, as a plan with a scale bar and north arrow. Returns a PNG to look at and the SVG it was drawn from. Each space and object is labelled from the first attribute named in `label_from` that holds a non-empty string, by default `common_name` (the family's nickname) and then `name_en`, with a leading \"the \" dropped and the first letter capitalized; without one it uses the row's `name` as stored. Pass `label_from: []` to label everything by `name`. Objects of kind `opening` cut a gap through the floor's walls at render time and are drawn as jamb ticks, with a dashed curve for an arch; the stored linework never changes."
    )]
    async fn render_plan(&self, Parameters(p): Parameters<RenderPlanParams>) -> Result<CallToolResult, ErrorData> {
        let size = p.size.unwrap_or(1600);
        if !(MIN_SIDE_PX..=MAX_SIDE_PX).contains(&size) {
            return Err(error::invalid(
                "size",
                format!("must be between {MIN_SIDE_PX} and {MAX_SIDE_PX}"),
            ));
        }
        let site = sqlx::query(
            "SELECT s.name, s.rotation, f.name AS floor FROM site s LEFT JOIN floor f ON f.id = $2 AND f.site_id = s.id AND f.deleted_at IS NULL
             WHERE s.id = $1 AND s.deleted_at IS NULL",
        )
        .bind(p.site_id)
        .bind(p.floor_id)
        .fetch_optional(&self.db)
        .await
        .map_err(error::db)?
        .ok_or_else(|| error::invalid("site_id", "no live site with this id"))?;
        let floor_name: Option<String> = site.get("floor");
        if p.floor_id.is_some() && floor_name.is_none() {
            return Err(error::invalid("floor_id", "no live floor with this id on this site"));
        }
        let highlight: HashSet<Highlight> = p.highlight.iter().copied().collect();
        let label_from: Vec<String> = match &p.label_from {
            Some(keys) => keys.clone(),
            None => DEFAULT_LABEL_FROM.map(String::from).to_vec(),
        };
        let openings = match p.floor_id {
            Some(floor_id) => self.openings(p.site_id, floor_id).await?,
            None => Vec::new(),
        };
        let mut features = Vec::new();
        if p.layers.linework
            && let Some(floor_id) = p.floor_id
        {
            features.extend(self.linework_features(floor_id, &openings).await?);
        }
        if p.layers.spaces {
            features.extend(self.space_features(&p, &highlight, &label_from).await?);
        }
        if p.layers.objects.as_ref().is_none_or(|kinds| !kinds.is_empty()) {
            let cutting: HashSet<i64> = openings.iter().map(|o| o.id).collect();
            features.extend(self.object_features(&p, &highlight, &label_from, &cutting).await?);
            if p.layers
                .objects
                .as_ref()
                .is_none_or(|kinds| kinds.iter().any(|k| k == "opening"))
            {
                features.extend(openings.into_iter().map(|o| Feature {
                    highlight: highlight.contains(&Highlight::Object(o.id)),
                    wkb: o.wkb,
                    style: Style::Opening {
                        from: o.from,
                        to: o.to,
                        wall_depth: o.wall_depth,
                        arch: o.arch,
                    },
                    label: None,
                }));
            }
        }
        if features.is_empty() {
            return Err(error::invalid(
                "layers",
                "nothing to draw on this floor with these layers",
            ));
        }
        let extent = self.extent(&features).await?;
        let site_name: String = site.get("name");
        let plan = Plan {
            title: match floor_name {
                Some(floor) => format!("{site_name}, {floor}"),
                None => format!("{site_name}, outdoors"),
            },
            rotation: site.get("rotation"),
            extent,
            features,
        };
        let rendered = tokio::task::spawn_blocking(move || render::render(&plan, size))
            .await
            .map_err(|e| error::internal("rendering a plan", e))?
            .map_err(|e| error::internal("rendering a plan", e))?;
        Ok(CallToolResult::success(vec![
            ContentBlock::image(STANDARD.encode(&rendered.png), "image/png"),
            ContentBlock::text(rendered.svg),
        ]))
    }
}

impl House {
    /// The live openings on a floor that can cut a wall: a two point
    /// LineString of some length. Any other shape is left to be drawn as an
    /// ordinary object, with a warning.
    async fn openings(&self, site_id: i64, floor_id: i64) -> Result<Vec<Opening>, ErrorData> {
        let rows = sqlx::query(
            "SELECT id, attributes, ST_AsBinary(geom) AS wkb, GeometryType(geom) AS shape, ST_NPoints(geom) AS points,
                    ST_X(ST_StartPoint(geom)) AS x0, ST_Y(ST_StartPoint(geom)) AS y0,
                    ST_X(ST_EndPoint(geom)) AS x1, ST_Y(ST_EndPoint(geom)) AS y1
             FROM object WHERE site_id = $1 AND floor_id = $2 AND kind = 'opening' AND deleted_at IS NULL",
        )
        .bind(site_id)
        .bind(floor_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let mut openings = Vec::new();
        for r in &rows {
            let id: i64 = r.get("id");
            let shape: String = r.get("shape");
            let points: Option<i32> = r.get("points");
            let ends = (
                r.get::<Option<f64>, _>("x0"),
                r.get::<Option<f64>, _>("y0"),
                r.get::<Option<f64>, _>("x1"),
                r.get::<Option<f64>, _>("y1"),
            );
            let (Some(x0), Some(y0), Some(x1), Some(y1)) = ends else {
                tracing::warn!("opening {id} is a {shape}, not a LineString, so it is drawn as an object");
                continue;
            };
            if shape != "LINESTRING" || points != Some(2) || (x0, y0) == (x1, y1) {
                tracing::warn!(
                    "opening {id} is a {shape} of {} points, not a LineString of two different points, so it is drawn as an object",
                    points.unwrap_or(0)
                );
                continue;
            }
            let attributes: Value = r.get("attributes");
            let wall_depth = attributes
                .get("wall_depth")
                .and_then(Value::as_f64)
                .filter(|d| d.is_finite() && *d > 0.0)
                .unwrap_or(DEFAULT_WALL_DEPTH);
            openings.push(Opening {
                id,
                wkb: r.get("wkb"),
                from: (x0, y0),
                to: (x1, y1),
                wall_depth,
                arch: attributes.get("opening_type").and_then(Value::as_str) == Some("arch"),
            });
        }
        Ok(openings)
    }

    /// The floor's linework, less a flat ended buffer of each opening, half
    /// its wall depth to each side. Rows an opening misses are drawn as
    /// stored, and nothing stored changes.
    async fn linework_features(&self, floor_id: i64, openings: &[Opening]) -> Result<Vec<Feature>, ErrorData> {
        let ids: Vec<i64> = openings.iter().map(|o| o.id).collect();
        let depths: Vec<f64> = openings.iter().map(|o| o.wall_depth).collect();
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            "WITH cut AS (
                 SELECT ST_Union(ST_Buffer(o.geom, c.depth / 2, 'endcap=flat')) AS area
                 FROM unnest($2::bigint[], $3::float8[]) AS c(id, depth) JOIN object o ON o.id = c.id
             ), drawn AS (
                 SELECT CASE WHEN ST_Intersects(l.geom, cut.area) THEN ST_Difference(l.geom, cut.area) ELSE l.geom END AS geom
                 FROM linework l CROSS JOIN cut
                 WHERE l.floor_id = $1 AND l.deleted_at IS NULL AND l.text IS NULL
             )
             SELECT ST_AsBinary(geom) FROM drawn WHERE NOT ST_IsEmpty(geom)",
        )
        .bind(floor_id)
        .bind(&ids)
        .bind(&depths)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        Ok(rows
            .into_iter()
            .map(|wkb| Feature {
                wkb,
                style: Style::Linework,
                label: None,
                highlight: false,
            })
            .collect())
    }

    async fn space_features(
        &self,
        p: &RenderPlanParams,
        highlight: &HashSet<Highlight>,
        label_from: &[String],
    ) -> Result<Vec<Feature>, ErrorData> {
        let rows = sqlx::query(
            "SELECT id, name, kind, attributes, ST_AsBinary(geom) AS wkb, ST_X(ST_PointOnSurface(geom)) AS lx, ST_Y(ST_PointOnSurface(geom)) AS ly
             FROM space WHERE site_id = $1 AND deleted_at IS NULL AND floor_id IS NOT DISTINCT FROM $2",
        )
        .bind(p.site_id)
        .bind(p.floor_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        Ok(rows
            .iter()
            .map(|r| Feature {
                wkb: r.get("wkb"),
                style: Style::Space { kind: r.get("kind") },
                label: label(r, p.layers.labels, label_from),
                highlight: highlight.contains(&Highlight::Space(r.get("id"))),
            })
            .collect())
    }

    async fn object_features(
        &self,
        p: &RenderPlanParams,
        highlight: &HashSet<Highlight>,
        label_from: &[String],
        cutting: &HashSet<i64>,
    ) -> Result<Vec<Feature>, ErrorData> {
        let rows = sqlx::query(
            "SELECT id, name, kind, attributes, ST_AsBinary(geom) AS wkb, ST_X(ST_PointOnSurface(geom)) AS lx, ST_Y(ST_PointOnSurface(geom)) AS ly
             FROM object WHERE site_id = $1 AND deleted_at IS NULL AND floor_id IS NOT DISTINCT FROM $2
               AND ($3::text[] IS NULL OR kind = ANY($3))",
        )
        .bind(p.site_id)
        .bind(p.floor_id)
        .bind(&p.layers.objects)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        Ok(rows
            .iter()
            .filter(|r| !cutting.contains(&r.get::<i64, _>("id")))
            .map(|r| Feature {
                wkb: r.get("wkb"),
                label: label(r, p.layers.labels && r.get::<&str, _>("kind") != "opening", label_from),
                style: Style::Object { kind: r.get("kind") },
                highlight: highlight.contains(&Highlight::Object(r.get("id"))),
            })
            .collect())
    }

    async fn extent(&self, features: &[Feature]) -> Result<Extent, ErrorData> {
        let geometries: Vec<&[u8]> = features.iter().map(|f| f.wkb.as_slice()).collect();
        let (xmin, ymin, xmax, ymax): (f64, f64, f64, f64) = sqlx::query_as(
            "SELECT ST_XMin(e), ST_YMin(e), ST_XMax(e), ST_YMax(e)
             FROM (SELECT ST_Extent(ST_GeomFromWKB(g, 0)) AS e FROM unnest($1::bytea[]) AS g) AS x",
        )
        .bind(&geometries)
        .fetch_one(&self.db)
        .await
        .map_err(error::db)?;
        Ok(Extent { xmin, ymin, xmax, ymax })
    }
}

/// A row's label at its label point. ST_PointOnSurface returns NULL for some
/// invalid polygons QGIS can store, and such a row is drawn without a label.
fn label(row: &PgRow, wanted: bool, label_from: &[String]) -> Option<Label> {
    let (x, y) = (row.get::<Option<f64>, _>("lx")?, row.get::<Option<f64>, _>("ly")?);
    wanted.then(|| Label {
        text: label_text(row.get("name"), &row.get::<Value, _>("attributes"), label_from),
        x,
        y,
    })
}

/// The first of `keys` whose attribute is a non-empty string, tidied, or
/// else `name` exactly as stored.
fn label_text(name: String, attributes: &Value, keys: &[String]) -> String {
    keys.iter()
        .find_map(|key| {
            attributes
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .map_or(name, tidy)
}

/// A nickname as a label: a leading "the " dropped when more follows, and
/// the first letter a capital.
fn tidy(text: &str) -> String {
    let text = match text.get(..4) {
        Some(article) if article.eq_ignore_ascii_case("the ") && !text[4..].trim().is_empty() => text[4..].trim_start(),
        _ => text,
    };
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn keys(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|k| (*k).to_owned()).collect()
    }

    #[test]
    fn labels_come_from_the_first_non_empty_string_attribute() {
        let defaults = keys(&DEFAULT_LABEL_FROM);
        let both = json!({"common_name": "the den", "name_en": "Study"});
        assert_eq!(label_text("Studio".into(), &both, &defaults), "Den");
        let english = json!({"name_en": "study"});
        assert_eq!(label_text("Studio".into(), &english, &defaults), "Study");
        let skipped = json!({"common_name": 4, "name_en": "  "});
        assert_eq!(label_text("studio".into(), &skipped, &defaults), "studio");
        assert_eq!(label_text("the studio".into(), &both, &[]), "the studio");
    }

    #[test]
    fn only_a_leading_article_with_more_after_it_is_dropped() {
        assert_eq!(tidy("THE  snug"), "Snug");
        assert_eq!(tidy("The"), "The");
        assert_eq!(tidy("the "), "The ");
        assert_eq!(tidy("theatre"), "Theatre");
        assert_eq!(tidy("über room"), "Über room");
    }
}

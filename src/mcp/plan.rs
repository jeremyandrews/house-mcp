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
        description = "Draw a floor, or the site's outdoor areas, as a plan with a scale bar and north arrow. Returns a PNG to look at and the SVG it was drawn from."
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
        let mut features = Vec::new();
        if p.layers.linework
            && let Some(floor_id) = p.floor_id
        {
            features.extend(self.linework_features(floor_id).await?);
        }
        if p.layers.spaces {
            features.extend(self.space_features(&p, &highlight).await?);
        }
        if p.layers.objects.as_ref().is_none_or(|kinds| !kinds.is_empty()) {
            features.extend(self.object_features(&p, &highlight).await?);
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
    async fn linework_features(&self, floor_id: i64) -> Result<Vec<Feature>, ErrorData> {
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT ST_AsBinary(geom) FROM linework WHERE floor_id = $1 AND deleted_at IS NULL AND text IS NULL",
        )
        .bind(floor_id)
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
    ) -> Result<Vec<Feature>, ErrorData> {
        let rows = sqlx::query(
            "SELECT id, name, kind, ST_AsBinary(geom) AS wkb, ST_X(ST_PointOnSurface(geom)) AS lx, ST_Y(ST_PointOnSurface(geom)) AS ly
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
                label: label(r, p.layers.labels),
                highlight: highlight.contains(&Highlight::Space(r.get("id"))),
            })
            .collect())
    }

    async fn object_features(
        &self,
        p: &RenderPlanParams,
        highlight: &HashSet<Highlight>,
    ) -> Result<Vec<Feature>, ErrorData> {
        let rows = sqlx::query(
            "SELECT id, name, kind, ST_AsBinary(geom) AS wkb, ST_X(ST_PointOnSurface(geom)) AS lx, ST_Y(ST_PointOnSurface(geom)) AS ly
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
            .map(|r| Feature {
                wkb: r.get("wkb"),
                style: Style::Object { kind: r.get("kind") },
                label: label(r, p.layers.labels),
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

/// A row's name at its label point. An empty geometry, which QGIS can store,
/// has no label point, so it is drawn without one.
fn label(row: &PgRow, wanted: bool) -> Option<Label> {
    let (x, y) = (row.get::<Option<f64>, _>("lx")?, row.get::<Option<f64>, _>("ly")?);
    wanted.then(|| Label {
        text: row.get("name"),
        x,
        y,
    })
}

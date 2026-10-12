//! Which spaces an opening joins, and the way from one space to another
//! through openings.

use std::collections::{HashMap, VecDeque};

use rmcp::{ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

use super::{
    House, json_result,
    plan::{opening_type, wall_depth},
};
use crate::error;

/// Straight pieces in the half disc that finds a space on one side of an
/// opening.
const HALF_DISC_SEGMENTS: u32 = 16;
/// Spaces that hold rooms rather than being one, so never the space on the
/// other side of a door.
const CONTAINER_KINDS: [&str; 2] = ["building", "parcel"];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectionsParams {
    /// Every opening that touches this space, and the space beyond each.
    pub space_id: Option<i64>,
    /// With `to_space`: the shortest way between the two through openings.
    pub from_space: Option<i64>,
    pub to_space: Option<i64>,
}

#[derive(Debug, Clone)]
struct Space {
    id: i64,
    name: String,
    kind: String,
    floor_id: Option<i64>,
}

impl Space {
    fn json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "kind": self.kind, "floor_id": self.floor_id })
    }
}

/// An opening and the one or two spaces it joins.
#[derive(Debug)]
struct Door {
    id: i64,
    name: String,
    opening_type: &'static str,
    sides: [Option<i64>; 2],
    /// `connects` when the attribute named the spaces, `geometry` when they
    /// were found from where the opening lies.
    from: &'static str,
}

impl Door {
    fn json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "opening_type": self.opening_type })
    }

    fn other_side(&self, space: i64) -> Option<Option<i64>> {
        match self.sides {
            [Some(a), b] if a == space => Some(b),
            [a, Some(b)] if b == space => Some(a),
            _ => None,
        }
    }
}

#[tool_router(router = connections_router, vis = "pub(super)")]
impl House {
    #[tool(
        description = "How spaces connect through openings (objects of kind `opening`: doors, arches and open doorways). Give `space_id` for every opening that touches that space, with its `opening_type` and the space on the other side (null when nothing recorded is there, such as outdoors). Give `from_space` and `to_space` for the shortest way between them, counted in openings passed, as an alternating list of spaces and openings; when no way exists through recorded openings, `path` is null and `message` says so. An opening joins the two spaces its `connects` attribute lists, such as `{\"connects\": [12, 13]}`; without that, the spaces either side of it on its floor within `wall_depth` of its midpoint. Example: {\"from_space\": 12, \"to_space\": 15}."
    )]
    async fn connections(&self, Parameters(p): Parameters<ConnectionsParams>) -> Result<CallToolResult, ErrorData> {
        let (first, last) = match (p.space_id, p.from_space, p.to_space) {
            (Some(space), None, None) => (space, None),
            (None, Some(from), Some(to)) => (from, Some(to)),
            (None, Some(_), None) => return Err(error::invalid("to_space", "is needed with from_space")),
            (None, None, Some(_)) => return Err(error::invalid("from_space", "is needed with to_space")),
            _ => {
                return Err(error::invalid(
                    "space_id",
                    "give either space_id, or from_space and to_space",
                ));
            }
        };
        let field = if last.is_some() { "from_space" } else { "space_id" };
        let site_id: i64 = sqlx::query_scalar("SELECT site_id FROM live_space WHERE id = $1")
            .bind(first)
            .fetch_optional(&self.db)
            .await
            .map_err(error::db)?
            .ok_or_else(|| error::invalid(field, "no live space with this id"))?;
        let spaces = self.site_spaces(site_id).await?;
        if let Some(to) = last
            && !spaces.contains_key(&to)
        {
            return Err(error::invalid(
                "to_space",
                "no live space with this id on the same site",
            ));
        }
        let (doors, warnings) = self.doors(site_id, &spaces).await?;
        let name = |id: Option<i64>| id.and_then(|id| spaces.get(&id)).map(Space::json);
        let mut result = match last {
            None => json!({
                "space": name(Some(first)),
                "connections": doors.iter().filter_map(|door| {
                    door.other_side(first).map(|other| json!({
                        "opening": door.json(),
                        "other_side": name(other),
                        "from": door.from,
                    }))
                }).collect::<Vec<_>>(),
            }),
            Some(to) => match shortest_path(&doors, first, to) {
                Some(steps) => {
                    let mut path = vec![json!({ "space": name(Some(first)) })];
                    for (door, space) in &steps {
                        path.push(json!({ "opening": doors[*door].json() }));
                        path.push(json!({ "space": name(Some(*space)) }));
                    }
                    json!({ "from": name(Some(first)), "to": name(Some(to)), "openings_passed": steps.len(), "path": path })
                }
                None => json!({
                    "from": name(Some(first)),
                    "to": name(Some(to)),
                    "path": null,
                    "message": format!(
                        "No path from {} to {} through recorded openings.",
                        spaces[&first].name, spaces[&to].name
                    ),
                }),
            },
        };
        if !warnings.is_empty() {
            result["warnings"] = json!(warnings);
        }
        json_result(result)
    }
}

impl House {
    async fn site_spaces(&self, site_id: i64) -> Result<HashMap<i64, Space>, ErrorData> {
        let rows = sqlx::query("SELECT id, name, kind, floor_id FROM live_space WHERE site_id = $1")
            .bind(site_id)
            .fetch_all(&self.db)
            .await
            .map_err(error::db)?;
        Ok(rows
            .iter()
            .map(|r| {
                let space = Space {
                    id: r.get("id"),
                    name: r.get("name"),
                    kind: r.get("kind"),
                    floor_id: r.get("floor_id"),
                };
                (space.id, space)
            })
            .collect())
    }

    /// Every live opening on the site with the spaces it joins, and a
    /// warning for each `connects` that could not be used.
    async fn doors(&self, site_id: i64, spaces: &HashMap<i64, Space>) -> Result<(Vec<Door>, Vec<String>), ErrorData> {
        let rows = sqlx::query(
            "SELECT id, name, floor_id, attributes, GeometryType(geom) = 'LINESTRING' AND ST_NPoints(geom) = 2 AS line,
                    ST_X(ST_StartPoint(geom)) AS x0, ST_Y(ST_StartPoint(geom)) AS y0,
                    ST_X(ST_EndPoint(geom)) AS x1, ST_Y(ST_EndPoint(geom)) AS y1
             FROM live_object WHERE site_id = $1 AND kind = 'opening' ORDER BY id",
        )
        .bind(site_id)
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        let mut doors = Vec::with_capacity(rows.len());
        let mut warnings = Vec::new();
        // The openings whose spaces come from geometry, as half discs either
        // side of the midpoint: (door index, side, floor, WKT).
        let mut probes: Vec<(usize, i32, i64, String)> = Vec::new();
        for r in &rows {
            let id: i64 = r.get("id");
            let attributes: Value = r.get("attributes");
            let mut door = Door {
                id,
                name: r.get("name"),
                opening_type: opening_type(&attributes),
                sides: [None, None],
                from: "geometry",
            };
            if let Some(connects) = attributes.get("connects") {
                match listed_spaces(connects, spaces) {
                    Ok(pair) => {
                        door.sides = pair.map(Some);
                        door.from = "connects";
                        doors.push(door);
                        continue;
                    }
                    Err(problem) => warnings.push(format!(
                        "opening {id}: connects {problem}, so its spaces were found from its geometry"
                    )),
                }
            }
            let floor: Option<i64> = r.get("floor_id");
            let ends = (
                r.get::<Option<f64>, _>("x0"),
                r.get::<Option<f64>, _>("y0"),
                r.get::<Option<f64>, _>("x1"),
                r.get::<Option<f64>, _>("y1"),
            );
            match (floor, r.get::<Option<bool>, _>("line"), ends) {
                (Some(floor), Some(true), (Some(x0), Some(y0), Some(x1), Some(y1))) if (x0, y0) != (x1, y1) => {
                    for (side, wkt) in [1, -1].into_iter().zip(half_discs((x0, y0), (x1, y1), wall_depth(&attributes))) {
                        probes.push((doors.len(), side, floor, wkt));
                    }
                }
                _ => warnings.push(format!(
                    "opening {id} is not a two point LineString on a floor and has no usable connects, so it joins nothing"
                )),
            }
            doors.push(door);
        }
        if !probes.is_empty() {
            self.find_sides(site_id, &probes, &mut doors).await?;
        }
        Ok((doors, warnings))
    }

    /// For each opening found by geometry, the space on each side: of the
    /// spaces with some area in that side's half disc, one that reaches only
    /// that side if there is one, and the smallest; never the same space for
    /// both sides, and never a building or parcel outline.
    async fn find_sides(
        &self,
        site_id: i64,
        probes: &[(usize, i32, i64, String)],
        doors: &mut [Door],
    ) -> Result<(), ErrorData> {
        let indexes: Vec<i64> = probes.iter().map(|p| p.0 as i64).collect();
        let sides: Vec<i32> = probes.iter().map(|p| p.1).collect();
        let floors: Vec<i64> = probes.iter().map(|p| p.2).collect();
        let shapes: Vec<&str> = probes.iter().map(|p| p.3.as_str()).collect();
        let rows = sqlx::query(
            "SELECT u.door, u.side, s.id, ST_Area(s.geom) AS area
             FROM unnest($1::bigint[], $2::int[], $3::bigint[], $4::text[]) AS u(door, side, floor_id, wkt)
             JOIN live_space s ON s.site_id = $5 AND s.floor_id = u.floor_id
                         AND s.kind <> ALL($6) AND ST_Intersects(s.geom, ST_GeomFromText(u.wkt, 0))
             WHERE ST_Area(ST_Intersection(s.geom, ST_GeomFromText(u.wkt, 0))) > 1e-9",
        )
        .bind(&indexes)
        .bind(&sides)
        .bind(&floors)
        .bind(&shapes)
        .bind(site_id)
        .bind(&CONTAINER_KINDS[..])
        .fetch_all(&self.db)
        .await
        .map_err(error::db)?;
        // Per door: (side, space, area).
        let mut found: HashMap<usize, Vec<(i32, i64, f64)>> = HashMap::new();
        for r in &rows {
            found.entry(r.get::<i64, _>("door") as usize).or_default().push((
                r.get("side"),
                r.get("id"),
                r.get("area"),
            ));
        }
        for (door, hits) in found {
            let on = |side: i32, space: i64| hits.iter().any(|h| h.0 == side && h.1 == space);
            let pick = |side: i32, not: Option<i64>| {
                hits.iter()
                    .filter(|h| h.0 == side && Some(h.1) != not)
                    .min_by(|a, b| on(-side, a.1).cmp(&on(-side, b.1)).then(a.2.total_cmp(&b.2)))
                    .map(|h| h.1)
            };
            let left = pick(1, None);
            let right = pick(-1, left);
            doors[door].sides = [left, right];
        }
        Ok(())
    }
}

/// The two space ids a `connects` attribute lists, when it is a list of two
/// different live spaces on the site.
fn listed_spaces(connects: &Value, spaces: &HashMap<i64, Space>) -> Result<[i64; 2], String> {
    let ids: Option<Vec<i64>> = connects
        .as_array()
        .map(|list| list.iter().filter_map(Value::as_i64).collect());
    match ids.as_deref() {
        Some(&[a, b]) if connects.as_array().map(Vec::len) == Some(2) => {
            if a == b {
                Err("names the same space twice".into())
            } else if let Some(missing) = [a, b].into_iter().find(|id| !spaces.contains_key(id)) {
                Err(format!("names {missing}, which is not a live space on this site"))
            } else {
                Ok([a, b])
            }
        }
        _ => Err("is not a list of exactly two space ids".into()),
    }
}

/// Half discs of radius `depth` on the left and then the right of the line
/// from `from` to `to`, round its midpoint, as WKT in the site frame.
fn half_discs(from: (f64, f64), to: (f64, f64), depth: f64) -> [String; 2] {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let length = dx.hypot(dy);
    let (ux, uy) = (dx / length, dy / length);
    let (mx, my) = ((from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0);
    [1.0, -1.0].map(|side: f64| {
        let (nx, ny) = (-uy * side, ux * side);
        let mut points: Vec<String> = (0..=HALF_DISC_SEGMENTS)
            .map(|i| {
                let t = std::f64::consts::PI * f64::from(i) / f64::from(HALF_DISC_SEGMENTS);
                let (along, across) = (depth * t.cos(), depth * t.sin());
                format!("{} {}", mx + ux * along + nx * across, my + uy * along + ny * across)
            })
            .collect();
        points.push(points[0].clone());
        format!("POLYGON(({}))", points.join(", "))
    })
}

/// The fewest openings from one space to another, breadth first, as the
/// opening passed and the space reached at each step. Empty when the two are
/// the same space; `None` when no way exists.
fn shortest_path(doors: &[Door], from: i64, to: i64) -> Option<Vec<(usize, i64)>> {
    let mut came_by: HashMap<i64, (usize, i64)> = HashMap::new();
    let mut queue = VecDeque::from([from]);
    let mut seen = std::collections::HashSet::from([from]);
    while let Some(space) = queue.pop_front() {
        if space == to {
            let mut steps = Vec::new();
            let mut at = to;
            while at != from {
                let (door, before) = came_by[&at];
                steps.push((door, at));
                at = before;
            }
            steps.reverse();
            return Some(steps);
        }
        for (i, door) in doors.iter().enumerate() {
            if let Some(Some(next)) = door.other_side(space)
                && seen.insert(next)
            {
                came_by.insert(next, (i, space));
                queue.push_back(next);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn door(id: i64, a: i64, b: i64) -> Door {
        Door {
            id,
            name: format!("door {id}"),
            opening_type: "door",
            sides: [Some(a), Some(b)],
            from: "connects",
        }
    }

    #[test]
    fn the_shortest_path_passes_the_fewest_openings() {
        // 1 - 2 - 3 - 4, and a long way round 1 - 5 - 6 - 4.
        let doors = [
            door(10, 1, 2),
            door(11, 2, 3),
            door(12, 3, 4),
            door(13, 1, 5),
            door(14, 5, 6),
            door(15, 6, 4),
            door(16, 2, 4),
        ];
        assert_eq!(shortest_path(&doors, 1, 4), Some(vec![(0, 2), (6, 4)]));
        assert_eq!(shortest_path(&doors, 4, 1), Some(vec![(6, 2), (0, 1)]));
        assert_eq!(shortest_path(&doors, 3, 3), Some(vec![]));
        assert_eq!(shortest_path(&doors, 1, 99), None);
    }

    #[test]
    fn an_opening_with_one_side_leads_nowhere() {
        let doors = [Door {
            sides: [Some(1), None],
            ..door(10, 1, 2)
        }];
        assert_eq!(doors[0].other_side(1), Some(None));
        assert_eq!(doors[0].other_side(3), None);
        assert_eq!(shortest_path(&doors, 1, 2), None);
    }

    #[test]
    fn connects_must_name_two_different_live_spaces() {
        let spaces: HashMap<i64, Space> = [1, 2]
            .map(|id| {
                (
                    id,
                    Space {
                        id,
                        name: format!("room {id}"),
                        kind: "room".into(),
                        floor_id: Some(1),
                    },
                )
            })
            .into();
        assert_eq!(listed_spaces(&json!([1, 2]), &spaces), Ok([1, 2]));
        assert!(listed_spaces(&json!([1, 1]), &spaces).is_err());
        assert!(listed_spaces(&json!([1, 3]), &spaces).is_err());
        assert!(listed_spaces(&json!([1, 2, 3]), &spaces).is_err());
        assert!(listed_spaces(&json!([1, "2"]), &spaces).is_err());
        assert!(listed_spaces(&json!("1,2"), &spaces).is_err());
    }

    #[test]
    fn half_discs_lie_left_then_right_of_the_line() {
        let [left, right] = half_discs((0.0, 0.0), (2.0, 0.0), 0.5);
        // The line runs east, so left is north: the top of the arc is at
        // y = 0.5 on the left and y = -0.5 on the right.
        assert!(left.contains("1 0.5"), "{left}");
        assert!(right.contains("1 -0.5"), "{right}");
    }
}

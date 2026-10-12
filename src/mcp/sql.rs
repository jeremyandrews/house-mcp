//! SQL fragments that more than one tool's statements share. Each is a macro
//! that expands to a string literal, so the statements that use them are
//! still literals put together at compile time.

/// Text in the form `find` compares names in: trimmed, without accents, in
/// lower case, and without a leading article (`the`, `il` or `la`).
macro_rules! normalised {
    ($text:literal) => {
        concat!(
            "regexp_replace(lower(unaccent(btrim(",
            $text,
            "))), '^(the|il|la)[[:space:]]+', '')"
        )
    };
}

/// A lateral join giving `in_space_id` and `in_space_name`: the smallest live
/// space on the object's floor, or outdoors for an outdoor object, that
/// covers the object's position. The position is the midpoint of a line and
/// the centroid of anything else, which for a point is the point itself. An
/// unplaced object has no position, so it is in no space.
macro_rules! in_space_join {
    ($object:literal) => {
        concat!(
            " LEFT JOIN LATERAL (SELECT c.id AS in_space_id, c.name AS in_space_name FROM live_space c WHERE ",
            $object,
            ".placement <> 'unplaced' AND c.site_id = ",
            $object,
            ".site_id AND c.floor_id IS NOT DISTINCT FROM ",
            $object,
            ".floor_id AND ST_Covers(c.geom, CASE WHEN GeometryType(",
            $object,
            ".geom) = 'LINESTRING' THEN ST_LineInterpolatePoint(",
            $object,
            ".geom, 0.5) ELSE ST_Centroid(",
            $object,
            ".geom) END) ORDER BY ST_Area(c.geom), c.id LIMIT 1) AS within ON true "
        )
    };
}

/// The names `find` searches on a row, as `(field, text, order)`: `name`,
/// then the string attributes `common_name`, `name_en`, `label` and
/// `nickname`, then each string in the attribute `nicknames`. The order
/// breaks a tie between two fields that match as well as each other.
macro_rules! searched_names {
    ($row:literal) => {
        concat!(
            "SELECT 'name'::text, ",
            $row,
            ".name, 0::bigint
             UNION ALL SELECT k.key, ",
            $row,
            ".attributes ->> k.key, k.ord
               FROM unnest(ARRAY['common_name', 'name_en', 'label', 'nickname']) WITH ORDINALITY AS k(key, ord)
               WHERE jsonb_typeof(",
            $row,
            ".attributes -> k.key) = 'string'
             UNION ALL SELECT 'nicknames', e #>> '{}', 5
               FROM jsonb_array_elements(CASE WHEN jsonb_typeof(",
            $row,
            ".attributes -> 'nicknames') = 'array' THEN ",
            $row,
            ".attributes -> 'nicknames' ELSE '[]' END) AS e
               WHERE jsonb_typeof(e) = 'string'"
        )
    };
}

/// A lateral join giving `matched_on` and `rank` for a row whose names are
/// listed by `$names`, against the normalised query `q.q`: rank 0 for an
/// exact match on any field, 1 for a prefix, 2 for a match anywhere, and no
/// row when nothing matches. `matched_on` is the field of the best match.
macro_rules! name_match {
    ($names:expr) => {
        concat!(
            " LEFT JOIN LATERAL (
                 SELECT n.field AS matched_on,
                        CASE WHEN n.t = q.q THEN 0 WHEN starts_with(n.t, q.q) THEN 1 ELSE 2 END AS rank
                 FROM (SELECT f.field, f.ord, ",
            normalised!("f.text"),
            " AS t FROM (",
            $names,
            ") AS f(field, text, ord)) AS n
                 WHERE strpos(n.t, q.q) > 0
                 ORDER BY rank, n.ord LIMIT 1
             ) AS m ON q.q IS NOT NULL "
        )
    };
}

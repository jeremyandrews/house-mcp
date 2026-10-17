-- Zoom levels both ways. Below an object: details, each a drawing in its own
-- frame, nested to any depth. Above a site: areas, such as a neighbourhood or
-- a town, nested to any depth.
--
-- Additive only: new tables, functions and views. The only changes to what
-- was here are a nullable dxf_import.detail_id, a nullable site.area_id and
-- the dxf_import_has_a_place check, replaced by one that also allows a
-- detail. No stored row is rewritten.

-- An area around sites. Its outline is WGS 84, stored as given; a site is in
-- an area when its area_id names it, or, with no area_id, when its origin
-- falls inside the outline.
CREATE TABLE area (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name            text NOT NULL CHECK (name <> '' AND length(name) <= 200),
    -- neighbourhood, hamlet, town, municipality, valley, region, or any
    -- other lowercase word.
    kind            text NOT NULL CHECK (kind ~ '^[a-z][a-z0-9_]{0,39}$'),
    parent_area_id  bigint REFERENCES area (id) CHECK (parent_area_id <> id),
    geom            geometry(Geometry, 4326) CHECK (
                        geom IS NULL OR (GeometryType(geom) IN ('POLYGON', 'MULTIPOLYGON') AND NOT ST_IsEmpty(geom))
                    ),
    attributes      jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(attributes) = 'object'),
    created_at      timestamptz NOT NULL DEFAULT now(),
    deleted_at      timestamptz
);
CREATE INDEX area_geom_gist ON area USING gist (geom);
CREATE INDEX area_parent ON area (parent_area_id);

ALTER TABLE site ADD COLUMN area_id bigint REFERENCES area (id);

-- Measurements: an object of up to width, depth and height, each
-- {"value": millimetres above zero, "source": measured, from_drawing or
-- estimated}, and nothing else.
CREATE FUNCTION house_detail_measurements_valid(m jsonb) RETURNS boolean
LANGUAGE sql IMMUTABLE
SET search_path = pg_catalog, public, pg_temp
AS $$
    SELECT CASE WHEN jsonb_typeof(m) <> 'object' THEN false ELSE NOT EXISTS (
        SELECT 1 FROM jsonb_each(m) AS e(key, value)
        WHERE CASE
            WHEN e.key NOT IN ('width', 'depth', 'height') OR jsonb_typeof(e.value) <> 'object' THEN true
            WHEN EXISTS (SELECT 1 FROM jsonb_object_keys(e.value) AS k WHERE k NOT IN ('value', 'source')) THEN true
            WHEN jsonb_typeof(e.value -> 'value') IS DISTINCT FROM 'number' THEN true
            WHEN (e.value ->> 'value')::numeric <= 0 THEN true
            ELSE coalesce(e.value ->> 'source', '') NOT IN ('measured', 'from_drawing', 'estimated')
        END
    ) END
$$;

-- A placement: numbers x and y, and optionally rotation and a scale above
-- zero, and nothing else.
CREATE FUNCTION house_detail_placement_valid(p jsonb) RETURNS boolean
LANGUAGE sql IMMUTABLE
SET search_path = pg_catalog, public, pg_temp
AS $$
    SELECT CASE
        WHEN jsonb_typeof(p) <> 'object' THEN false
        WHEN jsonb_typeof(p -> 'x') IS DISTINCT FROM 'number' OR jsonb_typeof(p -> 'y') IS DISTINCT FROM 'number' THEN false
        WHEN p ? 'rotation' AND jsonb_typeof(p -> 'rotation') <> 'number' THEN false
        WHEN p ? 'scale' AND jsonb_typeof(p -> 'scale') <> 'number' THEN false
        WHEN p ? 'scale' AND (p ->> 'scale')::numeric <= 0 THEN false
        ELSE NOT EXISTS (SELECT 1 FROM jsonb_object_keys(p) AS k WHERE k NOT IN ('x', 'y', 'rotation', 'scale'))
    END
$$;

-- A drawing of an object, or of part of another detail, in its own frame:
-- millimetres, x right, y up, origin at the drawing's lower left. The frame
-- has no relation to the site frame and no georeference.
CREATE TABLE detail (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    site_id             bigint NOT NULL REFERENCES site (id),
    object_id           bigint REFERENCES object (id),
    parent_detail_id    bigint REFERENCES detail (id) CHECK (parent_detail_id <> id),
    name                text NOT NULL CHECK (name <> '' AND length(name) <= 200),
    view                text NOT NULL CHECK (view IN ('top', 'front', 'side', 'section', 'exploded', 'other')),
    -- {"width": {"value": 2400, "source": "measured"}, ...}: millimetres,
    -- each of width, depth and height optional.
    measurements        jsonb CHECK (measurements IS NULL OR house_detail_measurements_valid(measurements)),
    -- {"x": .., "y": .., "rotation": degrees counterclockwise, "scale": ..}:
    -- where the detail's origin lies in its parent's frame and units. Null
    -- when the detail is linked to its parent but not drawn within it.
    placement           jsonb CHECK (placement IS NULL OR house_detail_placement_valid(placement)),
    attributes          jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(attributes) = 'object'),
    created_at          timestamptz NOT NULL DEFAULT now(),
    deleted_at          timestamptz,
    CONSTRAINT detail_has_one_parent CHECK (num_nonnulls(object_id, parent_detail_id) = 1)
);
CREATE INDEX detail_object ON detail (object_id);
CREATE INDEX detail_parent ON detail (parent_detail_id);

-- A labelled part of a detail, in the detail's frame. Attributes suit a
-- material, a finish or a note.
CREATE TABLE detail_part (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    detail_id       bigint NOT NULL REFERENCES detail (id),
    name            text NOT NULL CHECK (name <> '' AND length(name) <= 200),
    geom            geometry(Geometry, 0) NOT NULL CHECK (
                        GeometryType(geom) IN ('POINT', 'LINESTRING', 'POLYGON', 'MULTIPOINT', 'MULTILINESTRING', 'MULTIPOLYGON')
                        AND NOT ST_IsEmpty(geom)
                    ),
    attributes      jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(attributes) = 'object'),
    -- The import that made the part, so a second import of the same drawing
    -- replaces it, as for spaces.
    dxf_import_id   bigint REFERENCES dxf_import (id),
    deleted_at      timestamptz
);
CREATE INDEX detail_part_detail ON detail_part (detail_id);

-- A detail's imported CAD geometry. Its own table rather than linework, so
-- that no floor or site query can reach it by a missed filter.
CREATE TABLE detail_linework (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    dxf_import_id   bigint NOT NULL REFERENCES dxf_import (id),
    detail_id       bigint NOT NULL REFERENCES detail (id),
    site_id         bigint NOT NULL REFERENCES site (id),
    layer           text NOT NULL,
    entity          text NOT NULL,
    geom            geometry(Geometry, 0) NOT NULL CHECK (NOT ST_IsEmpty(geom)),
    text            text,
    text_height     double precision,
    deleted_at      timestamptz
);
CREATE INDEX detail_linework_detail ON detail_linework (detail_id);
CREATE INDEX detail_linework_import ON detail_linework (dxf_import_id);

-- An import onto a detail records the detail alone: dxf_import_site_live
-- keeps one live import per drawing and site among rows with no floor, which
-- a detail import with a site would collide with.
ALTER TABLE dxf_import
    ADD COLUMN detail_id bigint REFERENCES detail (id),
    DROP CONSTRAINT dxf_import_has_a_place,
    ADD CONSTRAINT dxf_import_has_a_place CHECK (
        (floor_id IS NOT NULL AND detail_id IS NULL)
        OR (floor_id IS NULL AND detail_id IS NULL AND site_id IS NOT NULL)
        OR (floor_id IS NULL AND site_id IS NULL AND detail_id IS NOT NULL)
    );
CREATE UNIQUE INDEX dxf_import_detail_live ON dxf_import (sha256, detail_id)
    WHERE deleted_at IS NULL AND detail_id IS NOT NULL;

-- Links from a file or a note to a detail or an area. They sit beside the
-- file and note tables rather than in them, so those tables are unchanged.
CREATE TABLE file_link (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    file_id     bigint NOT NULL UNIQUE REFERENCES file (id) ON DELETE CASCADE,
    detail_id   bigint REFERENCES detail (id),
    area_id     bigint REFERENCES area (id),
    CHECK (num_nonnulls(detail_id, area_id) > 0)
);
CREATE INDEX file_link_detail ON file_link (detail_id);
CREATE INDEX file_link_area ON file_link (area_id);

CREATE TABLE note_link (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    note_id     bigint NOT NULL UNIQUE REFERENCES note (id) ON DELETE CASCADE,
    detail_id   bigint REFERENCES detail (id),
    area_id     bigint REFERENCES area (id),
    CHECK (num_nonnulls(detail_id, area_id) > 0)
);
CREATE INDEX note_link_detail ON note_link (detail_id);
CREATE INDEX note_link_area ON note_link (area_id);

CREATE TRIGGER area_change AFTER INSERT OR UPDATE OR DELETE ON area FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER detail_change AFTER INSERT OR UPDATE OR DELETE ON detail FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER detail_part_change AFTER INSERT OR UPDATE OR DELETE ON detail_part FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER detail_linework_change AFTER INSERT OR UPDATE OR DELETE ON detail_linework FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER file_link_change AFTER INSERT OR UPDATE OR DELETE ON file_link FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER note_link_change AFTER INSERT OR UPDATE OR DELETE ON note_link FOR EACH ROW EXECUTE FUNCTION record_change();

-- An area may not be its own ancestor.
CREATE FUNCTION house_area_check_parent() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
    IF NEW.parent_area_id IS NOT NULL AND EXISTS (
        WITH RECURSIVE up(id) AS (
            SELECT NEW.parent_area_id
            UNION
            SELECT a.parent_area_id FROM area a JOIN up ON a.id = up.id WHERE a.parent_area_id IS NOT NULL
        )
        SELECT 1 FROM up WHERE up.id = NEW.id
    ) THEN
        RAISE EXCEPTION 'area % would be inside itself', NEW.id
            USING ERRCODE = 'check_violation', CONSTRAINT = 'area_no_cycle';
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER area_check_parent BEFORE INSERT OR UPDATE OF parent_area_id ON area
    FOR EACH ROW EXECUTE FUNCTION house_area_check_parent();

-- A detail may not be its own ancestor, and is on its parent's site, as its
-- children are on its own.
CREATE FUNCTION house_detail_check_parent() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    parent_site bigint;
BEGIN
    -- Not exactly one parent: detail_has_one_parent refuses the row.
    IF num_nonnulls(NEW.object_id, NEW.parent_detail_id) <> 1 THEN
        RETURN NEW;
    END IF;
    IF NEW.parent_detail_id IS NOT NULL AND EXISTS (
        WITH RECURSIVE up(id) AS (
            SELECT NEW.parent_detail_id
            UNION
            SELECT d.parent_detail_id FROM detail d JOIN up ON d.id = up.id WHERE d.parent_detail_id IS NOT NULL
        )
        SELECT 1 FROM up WHERE up.id = NEW.id
    ) THEN
        RAISE EXCEPTION 'detail % would be inside itself', NEW.id
            USING ERRCODE = 'check_violation', CONSTRAINT = 'detail_no_cycle';
    END IF;
    IF NEW.object_id IS NOT NULL THEN
        SELECT site_id INTO parent_site FROM object WHERE id = NEW.object_id;
    ELSE
        SELECT site_id INTO parent_site FROM detail WHERE id = NEW.parent_detail_id;
    END IF;
    IF parent_site IS DISTINCT FROM NEW.site_id THEN
        RAISE EXCEPTION 'detail % is on site %, its parent on site %', NEW.id, NEW.site_id, parent_site
            USING ERRCODE = 'check_violation', CONSTRAINT = 'detail_same_site';
    END IF;
    IF TG_OP = 'UPDATE' AND NEW.site_id <> OLD.site_id
       AND EXISTS (SELECT 1 FROM detail c WHERE c.parent_detail_id = NEW.id AND c.site_id <> NEW.site_id) THEN
        RAISE EXCEPTION 'detail % has children on site %', NEW.id, OLD.site_id
            USING ERRCODE = 'check_violation', CONSTRAINT = 'detail_same_site';
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER detail_check_parent BEFORE INSERT OR UPDATE ON detail
    FOR EACH ROW EXECUTE FUNCTION house_detail_check_parent();


-- Live, extending the definition in 0004: an area is live when neither it
-- nor any area it is inside is deleted.
CREATE VIEW live_area AS
SELECT a.*
FROM area a
WHERE a.deleted_at IS NULL
  AND NOT EXISTS (
      WITH RECURSIVE up(id) AS (
          SELECT a.parent_area_id WHERE a.parent_area_id IS NOT NULL
          UNION
          SELECT p.parent_area_id FROM area p JOIN up ON p.id = up.id WHERE p.parent_area_id IS NOT NULL
      )
      SELECT 1 FROM up JOIN area p ON p.id = up.id WHERE p.deleted_at IS NOT NULL
  );

-- A detail is live when neither it nor any detail it is inside is deleted,
-- and the object at the top of its chain is live, which takes in that
-- object's floor and site.
CREATE VIEW live_detail AS
SELECT d.*
FROM detail d
CROSS JOIN LATERAL (
    WITH RECURSIVE up(id, parent, object_id, deleted) AS (
        SELECT d.id, d.parent_detail_id, d.object_id, d.deleted_at IS NOT NULL
        UNION
        SELECT p.id, p.parent_detail_id, p.object_id, p.deleted_at IS NOT NULL
        FROM detail p JOIN up ON p.id = up.parent
    )
    SELECT bool_or(up.deleted) AS deleted, max(up.object_id) AS object_id FROM up
) AS chain
WHERE NOT chain.deleted
  AND EXISTS (SELECT 1 FROM live_object o WHERE o.id = chain.object_id);

CREATE VIEW live_detail_part AS
SELECT p.*
FROM detail_part p JOIN live_detail d ON d.id = p.detail_id
WHERE p.deleted_at IS NULL;

-- The area a site is directly in, and by which rule: `area_id` when the site
-- names a live area, or, with no area_id, `origin` for the smallest live
-- area whose outline covers the site's origin. No row when neither applies.
CREATE FUNCTION house_site_area(p_site_id bigint) RETURNS TABLE (area_id bigint, via text)
LANGUAGE sql STABLE
SET search_path = pg_catalog, public, pg_temp
AS $$
    SELECT a.id, 'area_id'::text
    FROM site s JOIN live_area a ON a.id = s.area_id
    WHERE s.id = p_site_id
    UNION ALL
    (SELECT a.id, 'origin'::text
     FROM site s JOIN live_area a ON a.geom IS NOT NULL AND ST_Covers(a.geom, ST_Transform(s.origin, 4326))
     WHERE s.id = p_site_id AND s.area_id IS NULL AND s.origin IS NOT NULL
     ORDER BY ST_Area(a.geom), a.id
     LIMIT 1)
$$;

GRANT SELECT ON area, detail, detail_part, detail_linework, file_link, note_link,
                live_area, live_detail, live_detail_part TO house_qgis, house_ro;
GRANT INSERT, UPDATE, DELETE ON area, detail, detail_part TO house_qgis;

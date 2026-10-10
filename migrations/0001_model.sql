-- The model of one or more sites. Geometry is in the site's own frame:
-- metres, x east, y north, SRID 0. Rows are never removed by the server; a
-- delete sets deleted_at, and every row change is copied into `change` by the
-- trigger at the end of this file.

CREATE TABLE site (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        text NOT NULL CHECK (name <> '' AND length(name) <= 200),
    -- The site frame's origin in WGS 84 / UTM zone 32N. Null until someone
    -- georeferences the site; the frame functions refuse such a site.
    origin      geometry(Point, 32632),
    -- Degrees clockwise from grid north to the site's y axis.
    rotation    double precision NOT NULL DEFAULT 0 CHECK (rotation > -360 AND rotation < 360),
    created_at  timestamptz NOT NULL DEFAULT now(),
    deleted_at  timestamptz
);
CREATE UNIQUE INDEX site_name_live ON site (name) WHERE deleted_at IS NULL;
CREATE INDEX site_origin_gist ON site USING gist (origin);

CREATE TABLE floor (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    site_id         bigint NOT NULL REFERENCES site (id),
    name            text NOT NULL CHECK (name <> '' AND length(name) <= 200),
    level           integer NOT NULL,
    elevation       double precision NOT NULL DEFAULT 0,
    ceiling_height  double precision CHECK (ceiling_height > 0),
    created_at      timestamptz NOT NULL DEFAULT now(),
    deleted_at      timestamptz,
    -- The target of the composite keys below, which stop a space or object
    -- from naming a floor on another site.
    UNIQUE (id, site_id)
);
CREATE UNIQUE INDEX floor_level_live ON floor (site_id, level) WHERE deleted_at IS NULL;

CREATE TABLE file (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        text NOT NULL CHECK (name <> '' AND length(name) <= 255),
    media_type  text NOT NULL CHECK (length(media_type) <= 255),
    size        bigint NOT NULL CHECK (size >= 0),
    sha256      text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    site_id     bigint REFERENCES site (id) ON DELETE SET NULL,
    floor_id    bigint REFERENCES floor (id) ON DELETE SET NULL,
    space_id    bigint,
    object_id   bigint,
    created_at  timestamptz NOT NULL DEFAULT now(),
    deleted_at  timestamptz
);
CREATE INDEX file_sha256 ON file (sha256);

-- One live import per file content and floor, which is what lets a second
-- import of the same drawing replace the first.
CREATE TABLE dxf_import (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    file_id     bigint NOT NULL REFERENCES file (id),
    sha256      text NOT NULL,
    floor_id    bigint NOT NULL REFERENCES floor (id),
    unit        text NOT NULL,
    options     jsonb NOT NULL,
    summary     jsonb NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    deleted_at  timestamptz
);
CREATE UNIQUE INDEX dxf_import_live ON dxf_import (sha256, floor_id) WHERE deleted_at IS NULL;

CREATE TABLE space (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    site_id         bigint NOT NULL REFERENCES site (id),
    -- Null for an outdoor area.
    floor_id        bigint,
    name            text NOT NULL CHECK (name <> '' AND length(name) <= 200),
    kind            text NOT NULL CHECK (kind IN ('room', 'terrace', 'garden_bed', 'pool', 'parcel', 'building')),
    geom            geometry(Polygon, 0) NOT NULL CHECK (NOT ST_IsEmpty(geom)),
    wall_material   text,
    attributes      jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(attributes) = 'object'),
    dxf_import_id   bigint REFERENCES dxf_import (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    deleted_at      timestamptz,
    FOREIGN KEY (floor_id, site_id) REFERENCES floor (id, site_id)
);
CREATE INDEX space_geom_gist ON space USING gist (geom);
CREATE INDEX space_floor ON space (floor_id);

CREATE TABLE object (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    site_id         bigint NOT NULL REFERENCES site (id),
    -- Null for something outdoors, such as a tree or a buried pipe.
    floor_id        bigint,
    -- light, switch, socket, access_point, camera, sensor, speaker, tree,
    -- pipe, cable_run, or any other lowercase word.
    kind            text NOT NULL CHECK (kind ~ '^[a-z][a-z0-9_]{0,39}$'),
    name            text NOT NULL CHECK (name <> '' AND length(name) <= 200),
    geom            geometry(Geometry, 0) NOT NULL CHECK (
                        GeometryType(geom) IN ('POINT', 'LINESTRING', 'POLYGON', 'MULTIPOINT', 'MULTILINESTRING', 'MULTIPOLYGON')
                        AND NOT ST_IsEmpty(geom)
                    ),
    -- Metres above the floor, or above the ground outdoors.
    height          double precision,
    -- Identifiers in other systems: ha_entity_id, unifi_mac and the like.
    refs            jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(refs) = 'object'),
    attributes      jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(attributes) = 'object'),
    dxf_import_id   bigint REFERENCES dxf_import (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    deleted_at      timestamptz,
    FOREIGN KEY (floor_id, site_id) REFERENCES floor (id, site_id)
);
CREATE INDEX object_geom_gist ON object USING gist (geom);
CREATE INDEX object_floor ON object (floor_id);
CREATE INDEX object_refs ON object USING gin (refs);

ALTER TABLE file
    ADD FOREIGN KEY (space_id) REFERENCES space (id) ON DELETE SET NULL,
    ADD FOREIGN KEY (object_id) REFERENCES object (id) ON DELETE SET NULL;

-- CAD geometry as drawn, kept for rendering. Text entities keep their string
-- so that an import can name the spaces they sit in.
CREATE TABLE linework (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    dxf_import_id   bigint NOT NULL REFERENCES dxf_import (id),
    floor_id        bigint NOT NULL REFERENCES floor (id),
    layer           text NOT NULL,
    entity          text NOT NULL,
    geom            geometry(Geometry, 0) NOT NULL CHECK (NOT ST_IsEmpty(geom)),
    text            text,
    text_height     double precision,
    deleted_at      timestamptz
);
CREATE INDEX linework_geom_gist ON linework USING gist (geom);
CREATE INDEX linework_import ON linework (dxf_import_id);

CREATE TABLE note (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    body        text NOT NULL CHECK (body <> '' AND length(body) <= 100000),
    source      text NOT NULL DEFAULT '' CHECK (length(source) <= 1000),
    site_id     bigint REFERENCES site (id) ON DELETE SET NULL,
    floor_id    bigint REFERENCES floor (id) ON DELETE SET NULL,
    space_id    bigint REFERENCES space (id) ON DELETE SET NULL,
    object_id   bigint REFERENCES object (id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    deleted_at  timestamptz
);

CREATE TABLE change (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at          timestamptz NOT NULL DEFAULT clock_timestamp(),
    author      text NOT NULL,
    client      text NOT NULL,
    reason      text NOT NULL,
    table_name  text NOT NULL,
    row_id      bigint NOT NULL,
    operation   text NOT NULL CHECK (operation IN ('INSERT', 'UPDATE', 'DELETE')),
    before      jsonb,
    after       jsonb
);
CREATE INDEX change_row ON change (table_name, row_id, id);

-- The author and reason come from settings the server makes local to each
-- transaction. Only a session logged in as the owning role may name an
-- author; any other session is recorded as its own login role, so a QGIS
-- user cannot sign a change as someone else. Any session may give a reason.
CREATE FUNCTION record_change() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    old_row jsonb;
    new_row jsonb;
    who text := session_user;
BEGIN
    IF TG_OP <> 'INSERT' THEN
        old_row := to_jsonb(OLD);
    END IF;
    IF TG_OP <> 'DELETE' THEN
        new_row := to_jsonb(NEW);
    END IF;
    IF TG_OP = 'UPDATE' AND old_row = new_row THEN
        RETURN NULL;
    END IF;
    IF session_user = current_user THEN
        who := coalesce(nullif(current_setting('house.author', true), ''), session_user);
    END IF;
    INSERT INTO public.change (author, client, reason, table_name, row_id, operation, before, after)
    VALUES (
        who,
        coalesce(current_setting('application_name', true), ''),
        coalesce(current_setting('house.reason', true), ''),
        TG_TABLE_NAME,
        (coalesce(new_row, old_row) ->> 'id')::bigint,
        TG_OP,
        old_row,
        new_row
    );
    RETURN NULL;
END
$$;
REVOKE ALL ON FUNCTION record_change() FROM PUBLIC;

CREATE TRIGGER site_change AFTER INSERT OR UPDATE OR DELETE ON site FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER floor_change AFTER INSERT OR UPDATE OR DELETE ON floor FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER file_change AFTER INSERT OR UPDATE OR DELETE ON file FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER dxf_import_change AFTER INSERT OR UPDATE OR DELETE ON dxf_import FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER space_change AFTER INSERT OR UPDATE OR DELETE ON space FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER object_change AFTER INSERT OR UPDATE OR DELETE ON object FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER linework_change AFTER INSERT OR UPDATE OR DELETE ON linework FOR EACH ROW EXECUTE FUNCTION record_change();
CREATE TRIGGER note_change AFTER INSERT OR UPDATE OR DELETE ON note FOR EACH ROW EXECUTE FUNCTION record_change();

-- From any EPSG coordinate system into the site frame: to UTM 32N, move the
-- origin to zero, then turn grid north onto the site's y axis.
CREATE FUNCTION house_to_local(g geometry, p_site_id bigint) RETURNS geometry
LANGUAGE plpgsql STABLE
SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    o geometry;
    r double precision;
BEGIN
    IF ST_SRID(g) = 0 THEN
        RAISE EXCEPTION 'geometry has no SRID to convert from' USING ERRCODE = '22023';
    END IF;
    SELECT s.origin, radians(s.rotation) INTO o, r FROM site s WHERE s.id = p_site_id;
    IF o IS NULL THEN
        RAISE EXCEPTION 'site % has no georeference', p_site_id USING ERRCODE = '22023';
    END IF;
    RETURN ST_SetSRID(
        ST_Affine(
            ST_Translate(ST_Transform(g, 32632), -ST_X(o), -ST_Y(o)),
            cos(r), -sin(r), sin(r), cos(r), 0, 0
        ),
        0
    );
END
$$;

-- The inverse of house_to_local, into any EPSG coordinate system.
CREATE FUNCTION house_from_local(g geometry, p_site_id bigint, p_srid integer DEFAULT 32632) RETURNS geometry
LANGUAGE plpgsql STABLE
SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    o geometry;
    r double precision;
BEGIN
    SELECT s.origin, radians(s.rotation) INTO o, r FROM site s WHERE s.id = p_site_id;
    IF o IS NULL THEN
        RAISE EXCEPTION 'site % has no georeference', p_site_id USING ERRCODE = '22023';
    END IF;
    RETURN ST_Transform(
        ST_SetSRID(
            ST_Translate(ST_Affine(ST_SetSRID(g, 0), cos(r), sin(r), -sin(r), cos(r), 0, 0), ST_X(o), ST_Y(o)),
            32632
        ),
        p_srid
    );
END
$$;

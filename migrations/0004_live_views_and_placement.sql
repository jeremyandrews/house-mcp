-- Additive only: an extension, one defaulted column filled from an existing
-- attribute, and three views. Nothing is dropped and no geometry changes.

-- unaccent is a trusted extension, so the database owner may create it.
-- find uses it to match names whatever their accents.
CREATE EXTENSION IF NOT EXISTS unaccent;

-- How an object's geometry was found. `exact`: placed where it is.
-- `approximate`: somewhere near, such as the middle of its room. `unplaced`:
-- not known at all; the geometry is a stand in, since geom may not be null.
ALTER TABLE object
    ADD COLUMN placement text NOT NULL DEFAULT 'exact'
        CONSTRAINT object_placement CHECK (placement IN ('exact', 'approximate', 'unplaced'));

-- Objects seeded before this column recorded how they were placed in the
-- `position` attribute, which stays as it is. The fill is recorded in
-- `change` like any other write.
SELECT set_config('house.author', 'migration 0004', true),
       set_config('house.reason', 'placement from the position attribute', true);

DO $$
DECLARE
    counts record;
BEGIN
    FOR counts IN
        SELECT attributes ->> 'position' AS position, count(*) AS n
        FROM object WHERE attributes ->> 'position' IS NOT NULL
        GROUP BY 1 ORDER BY 1
    LOOP
        RAISE NOTICE 'placement fill: position % on % objects, placement %', counts.position, counts.n,
            CASE WHEN counts.position = 'site_unplaced' THEN 'unplaced' ELSE 'approximate' END;
    END LOOP;
END
$$;

UPDATE object
SET placement = CASE WHEN attributes ->> 'position' = 'site_unplaced' THEN 'unplaced' ELSE 'approximate' END
WHERE attributes ->> 'position' IS NOT NULL;

-- The one definition of live that every read uses: a row is live when it is
-- not deleted, and neither is its site, nor its floor when it has one. A
-- delete marks only the row named, so restoring a site or floor brings back
-- what it held as it was. Each view lists the table's columns as they are
-- now; a migration that adds a column to the table recreates its view.
CREATE VIEW live_floor AS
SELECT f.*
FROM floor f JOIN site s ON s.id = f.site_id
WHERE f.deleted_at IS NULL AND s.deleted_at IS NULL;

CREATE VIEW live_space AS
SELECT sp.*
FROM space sp
JOIN site s ON s.id = sp.site_id
LEFT JOIN floor f ON f.id = sp.floor_id
WHERE sp.deleted_at IS NULL AND s.deleted_at IS NULL
  AND (sp.floor_id IS NULL OR f.deleted_at IS NULL);

CREATE VIEW live_object AS
SELECT o.*
FROM object o
JOIN site s ON s.id = o.site_id
LEFT JOIN floor f ON f.id = o.floor_id
WHERE o.deleted_at IS NULL AND s.deleted_at IS NULL
  AND (o.floor_id IS NULL OR f.deleted_at IS NULL);

GRANT SELECT ON live_floor, live_space, live_object TO house_qgis, house_ro;

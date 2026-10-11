-- Additive only: two defaulted columns, two nullable columns, two relaxed NOT
-- NULLs and an index. No stored row is rewritten.

-- Free form facts about a site or a floor, such as where its drawings came
-- from, as spaces and objects already have.
ALTER TABLE site
    ADD COLUMN attributes jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(attributes) = 'object');
ALTER TABLE floor
    ADD COLUMN attributes jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(attributes) = 'object');

-- A drawing of the grounds belongs to a site rather than a floor. Such an
-- import, and its linework, has a site and no floor. An import onto a floor
-- records its site too from now on; older rows keep a null site_id, which
-- their floor still answers.
ALTER TABLE dxf_import
    ADD COLUMN site_id bigint REFERENCES site (id),
    ALTER COLUMN floor_id DROP NOT NULL,
    ADD CONSTRAINT dxf_import_has_a_place CHECK (floor_id IS NOT NULL OR site_id IS NOT NULL);
-- dxf_import_live leaves a null floor unconstrained, so a second import of
-- the same drawing onto the same site is kept to one live row here.
CREATE UNIQUE INDEX dxf_import_site_live ON dxf_import (sha256, site_id) WHERE deleted_at IS NULL AND floor_id IS NULL;

ALTER TABLE linework
    ADD COLUMN site_id bigint REFERENCES site (id),
    ALTER COLUMN floor_id DROP NOT NULL,
    ADD CONSTRAINT linework_has_a_place CHECK (floor_id IS NOT NULL OR site_id IS NOT NULL);
CREATE INDEX linework_site_outdoors ON linework (site_id) WHERE floor_id IS NULL;

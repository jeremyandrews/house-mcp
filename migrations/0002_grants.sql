-- house_qgis and house_ro are created outside this schema (by the database
-- operator in production, by the test harness in tests), because the owning
-- role cannot create roles. This migration fails if either is missing.

GRANT USAGE ON SCHEMA public TO house_qgis, house_ro;

GRANT SELECT ON site, floor, space, object, note, file, dxf_import, linework, change TO house_qgis, house_ro;
GRANT INSERT, UPDATE, DELETE ON site, floor, space, object, note TO house_qgis;

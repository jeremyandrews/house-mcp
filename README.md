# house-mcp

An MCP server that holds a structured model of a house and its grounds in PostGIS: floors, rooms, devices, trees, boundaries, files and notes. It answers spatial questions about the model, renders plans as images, and lets an agent add to the model as knowledge arrives. The same tables can be edited directly in QGIS, and every change from any client is recorded.

This repository holds code only. No house data, file or coordinate is ever committed to it. Test fixtures are synthetic and are written by the tests themselves.

## The model

Each site has its own frame: metres, x east, y north, z up, with an origin chosen by whoever creates the site. All model geometry is stored in that frame with SRID 0. A site may also carry a georeference: its origin as a point in EPSG:32632 (WGS 84 / UTM zone 32N), and the angle in degrees clockwise from grid north to its y axis. Two SQL functions convert between the site frame and any EPSG coordinate system. `house_to_local(geom, site_id)` takes a geometry that carries an SRID; `house_from_local(geom, site_id, srid)` returns one in `srid`, which defaults to 32632.

| Table | Holds |
| --- | --- |
| `site` | name, optional origin and rotation |
| `floor` | site, name, level, elevation, ceiling height |
| `space` | site, floor (null outdoors), name, kind (`room`, `terrace`, `garden_bed`, `pool`, `parcel`, `building`), polygon, wall material, attributes |
| `object` | site, floor (null outdoors), kind (`light`, `switch`, `socket`, `access_point`, `camera`, `sensor`, `speaker`, `tree`, `pipe`, `cable_run` or any other lowercase word), name, point, line or polygon, height above the floor, outside identifiers in `refs`, attributes |
| `linework` | CAD geometry as drawn, by import and layer, for rendering |
| `dxf_import` | one row per import of a drawing onto a floor |
| `file` | name, media type, size, sha256, optional links to a site, floor, space or object |
| `note` | markdown text with a source line, linked like a file |
| `change` | every insert, update and delete on the tables above |

The server never removes a row. A delete sets `deleted_at`, and `restore` clears it.

### The change log

Triggers on every model table write a `change` row for each insert, update and delete, whichever client made it: time, author, client (`application_name`), reason, operation, and the row before and after as `jsonb`. The server sets the author and reason for each transaction with `set_config(..., true)`, so they end with the transaction. A session logged in as any other role is recorded under that role, even if it sets an author of its own; any session may set `house.reason`. A row removed outright, from QGIS or `psql`, keeps its last state in `change`, which is what `restore` puts back.

### Roles

| Role | May |
| --- | --- |
| `house` | own everything; the server's role, and the only one that runs migrations |
| `house_qgis` | select, insert, update and delete on `site`, `floor`, `space`, `object` and `note`; select on everything else; never write `change` |
| `house_ro` | select on everything |

The migrations grant to `house_qgis` and `house_ro` but do not create them, because the owning role cannot create roles. Both must exist before the server first starts, and the database needs the `postgis` extension, which only a superuser can create.

## Tools

Read tools, available to both tokens:

| Tool | Does |
| --- | --- |
| `list_sites` | live sites with their floors and georeference |
| `get_floor` | a floor with its spaces and objects as GeoJSON |
| `find` | spaces and objects by part of a name, kind, outside identifier or attribute |
| `what_is_at` | the spaces containing a point, and the objects within a radius |
| `in_space` | the objects and smaller spaces inside a space |
| `nearest` | the n objects of a kind nearest a point, space or object, in plan |
| `measure` | area, perimeter and length, and the distance between two things |
| `list_files` | files, optionally by what they are linked to |
| `get_file` | a file's metadata, with its content when it is UTF-8 text of at most 1 MiB |
| `search_notes` | notes containing some text |
| `history` | the `change` rows for one thing |
| `render_plan` | a floor or the site's outdoor areas as a PNG and the SVG it was drawn from, with a scale bar and north arrow |

Write tools take a required `reason`. They are refused to the read only token and left out of its tool list.

| Tool | Does |
| --- | --- |
| `create_site` | a site, optionally with an origin in any EPSG system and a rotation |
| `upsert_floor`, `upsert_space`, `upsert_object` | create a row, or replace the fields of an existing one |
| `set_attributes` | add, change or remove some attributes of a space or object |
| `link_ref` | set or remove one outside identifier on an object, such as `ha_entity_id` |
| `add_note` | a markdown note with its source |
| `upload_file` | text inline, or binary as base64 up to 25 MiB |
| `import_geojson` | features in any EPSG system as spaces or objects, converted into the site frame |
| `import_dxf` | a stored DXF drawing onto a floor |
| `delete`, `restore` | mark a row deleted, or bring it back |

### DXF import

`import_dxf` reads LINE, LWPOLYLINE, POLYLINE, ARC, CIRCLE, INSERT (blocks expanded), TEXT and MTEXT from model space. Every entity read is kept as linework. A layer mapping such as `{"A-ROOM": {"space": "room"}, "E-LIGHT": {"object": "light"}}` turns closed polylines on a space layer into spaces, each named by the tallest text that PostGIS finds inside it, and block insertions on an object layer into objects at their insertion points. Layer names match in any case.

Units come from the drawing's `$INSUNITS` header. A drawing with no unit there is refused unless the call gives `unit`, which also overrides the header when both are present. An optional `transform` scales, rotates (degrees counterclockwise) and offsets the drawing after the unit conversion. The result reports counts per layer, the spaces and objects made, outlines PostGIS found invalid, and every entity skipped with the reason.

Importing the same file content onto the same floor again marks the earlier import and everything it made as deleted, then imports afresh. A DWG file can be stored but not imported; convert it to DXF with the free ODA File Converter first.

### Plan labels

`render_plan` labels each space and object from the first attribute in `label_from` that holds a non-empty string, and otherwise from the row's `name` exactly as stored. `label_from` defaults to `["common_name", "name_en"]`: the family's nickname first, then the English name. A label taken from an attribute loses a leading "the " and starts with a capital, so a `common_name` of `the snug` is drawn as `Snug`. An empty list labels everything by `name`:

```json
{"site_id": 1, "floor_id": 2, "label_from": []}
```

Nicknames are ordinary attributes, so `set_attributes` changes a label on the next render:

```json
{"kind": "space", "id": 7, "set": {"common_name": "the snug"}, "reason": "what we call it"}
```

### Openings

An object of kind `opening` is an arch or open doorway. Its geometry is a LineString of exactly two points along the wall's centre line, from one jamb to the other, on the opening's floor. When `render_plan` draws that floor it removes the floor's linework within a flat ended buffer of the line, half of `wall_depth` to each side, and draws a jamb tick across the wall at each end; an arch also gets a dashed half ellipse across the gap. The stored linework is never changed, so moving or deleting the opening shows on the next render.

| Attribute | Value | Default |
| --- | --- | --- |
| `opening_type` | `arch` or `opening` | `opening` |
| `wall_depth` | the wall's thickness in metres, a positive number | 0.6 |

Any other geometry, or a LineString of more than two points, is drawn as an ordinary object and logged as a warning.

```json
{"site_id": 1, "floor_id": 2, "kind": "opening", "name": "Arch to the hall",
 "geometry": {"type": "LineString", "coordinates": [[4.0, 1.2], [4.0, 2.8]]},
 "attributes": {"opening_type": "arch", "wall_depth": 0.45}, "reason": "arch on the survey"}
```

### Limits

| What | Limit |
| --- | --- |
| MCP request body | 36 MiB |
| file through a tool call | 25 MiB |
| request body through HTTP, which for an upload is the file | 100 MiB plus 64 KiB |
| stored file an import reads (DXF or GeoJSON) | 16 MiB |
| DXF entities, counting every copy a block expands to | 100,000 |
| DXF vertices, counted the same way | 2,000,000 |
| DXF block nesting | 16 levels; a block that inserts itself is refused |
| vertices in one geometry | 10,000 |
| GeoJSON features in one import | 10,000 |
| render size | 200 to 4096 pixels on the long side |

## HTTP

| Route | Auth | Does |
| --- | --- | --- |
| `GET /healthz` | none | 200 when the database answers |
| `POST /mcp` | bearer | MCP over streamable HTTP, stateless |
| `POST /api/files?name=...` | bearer, read and write | stores the request body; `Content-Type` is the media type; optional `reason`, `site_id`, `floor_id`, `space_id`, `object_id` |
| `GET /api/files/<sha256>` | bearer | the file, as an attachment |
| `GET /` and `POST /` | basic, with the read and write token as the password | a page listing files with an upload form, for a phone |
| `GET /files/<sha256>` | basic, as above | the file, as an attachment |

Files are stored on the volume under their sha256 and are only ever addressed by it. Downloads are served as `application/octet-stream` attachments with `nosniff`. The page escapes every name, sends a strict Content Security Policy and refuses a form post whose `Origin` is not its own.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATABASE_URL` | | the `house` role's connection string |
| `HOUSE_TOKEN_RW`, `HOUSE_TOKEN_RW_NAME` | | the read and write token, at least 32 characters, and the author name its changes carry |
| `HOUSE_TOKEN_RO`, `HOUSE_TOKEN_RO_NAME` | | the read only token, also at least 32 characters and different from the other, and its name |
| `HOUSE_FILES_DIR` | `/data/files` | where files are stored |
| `HOUSE_LISTEN` | `0.0.0.0:8080` | the address to listen on |
| `HOUSE_ALLOWED_HOSTS` | `house.pozza,localhost,127.0.0.1` | host names the MCP endpoint answers to, comma separated |
| `RUST_LOG` | `info` | log filter |

## Tests

The database and end to end tests need PostGIS 3.6 on Postgres 16, and a superuser URL for it in `HOUSE_TEST_DATABASE_URL`. Each test makes its own database. CI runs the image production runs:

```sh
docker run -d --name house-pg -p 55432:5432 ghcr.io/cloudnative-pg/postgis:16.15-3.6.4-standard-trixie \
  bash -c 'initdb -D /tmp/pg -U postgres --auth=trust >/dev/null &&
    echo "host all all all trust" >> /tmp/pg/pg_hba.conf &&
    exec postgres -D /tmp/pg -c listen_addresses="*" -c unix_socket_directories=/tmp'
HOUSE_TEST_DATABASE_URL=postgres://postgres@localhost:55432/postgres cargo test
```

## Image

The `Containerfile` builds a release binary on the official Rust image and copies it into a slim Debian image that runs as UID 10001 on port 8080. The cluster's nodes are arm64, so it is built on one of them:

```sh
TAG=$(date +%Y%m%d)
sudo docker build --platform linux/arm64 -f Containerfile \
  -t ghcr.io/jeremyandrews/house-mcp:$TAG -t registry.pozza/jeremyandrews/house-mcp:$TAG .
sudo docker push ghcr.io/jeremyandrews/house-mcp:$TAG
sudo docker push registry.pozza/jeremyandrews/house-mcp:$TAG
```

The label font is Noto Sans Regular, embedded in the binary so a plan renders the same everywhere.

## Licence

MIT, except `assets/NotoSans-Regular.ttf`, which is under the SIL Open Font License 1.1 in `assets/OFL.txt`.

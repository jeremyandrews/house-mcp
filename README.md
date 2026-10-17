# house-mcp

An MCP server that holds a structured model of a house and its grounds in PostGIS: floors, rooms, devices, trees, boundaries, files and notes. It answers spatial questions about the model, renders plans as images, and lets an agent add to the model as knowledge arrives. The same tables can be edited directly in QGIS, and every change from any client is recorded.

This repository holds code only. No house data, file or coordinate is ever committed to it. Test fixtures are synthetic and are written by the tests themselves.

## The model

Each site has its own frame: metres, x east, y north, z up, with an origin chosen by whoever creates the site. All model geometry is stored in that frame with SRID 0. A site may also carry a georeference: its origin as a point in EPSG:32632 (WGS 84 / UTM zone 32N), and the angle in degrees clockwise from grid north to its y axis. Two SQL functions convert between the site frame and any EPSG coordinate system. `house_to_local(geom, site_id)` takes a geometry that carries an SRID; `house_from_local(geom, site_id, srid)` returns one in `srid`, which defaults to 32632.

| Table | Holds |
| --- | --- |
| `area` | an area around sites (neighbourhood, town, region, any lowercase `kind`), optional parent area, optional outline in WGS 84, attributes |
| `site` | name, optional origin and rotation, optional area, attributes |
| `floor` | site, name, level, elevation, ceiling height, attributes |
| `space` | site, floor (null outdoors), name, kind (`room`, `terrace`, `garden_bed`, `pool`, `parcel`, `building`), polygon, wall material, attributes |
| `object` | site, floor (null outdoors), kind (`light`, `switch`, `socket`, `access_point`, `camera`, `sensor`, `speaker`, `tree`, `pipe`, `cable_run`, `opening` or any other lowercase word), name, point, line or polygon, `placement` (`exact`, `approximate` or `unplaced`), height above the floor, outside identifiers in `refs`, attributes |
| `detail` | a drawing of an object, or of part of another detail, in its own millimetre frame: site, exactly one parent (`object_id` or `parent_detail_id`), name, `view` (`top`, `front`, `side`, `section`, `exploded`, `other`), optional `measurements` with their sources, optional `placement` in the parent, attributes |
| `detail_part` | a labelled part of a detail, any 2D geometry in the detail's frame, attributes such as a material or finish |
| `linework` | CAD geometry as drawn, by import and layer, for rendering; on a floor, or on the site with no floor |
| `detail_linework` | the same for a detail, in its own table so that no floor or site query can reach it |
| `dxf_import` | one row per import of a drawing onto a floor, onto a site with no floor, or onto a detail |
| `file` | name, media type, size, sha256, optional links to a site, floor, space or object |
| `note` | markdown text with a source line, linked like a file |
| `file_link`, `note_link` | a file's or note's link to a detail or an area, beside the file and note tables |
| `change` | every insert, update and delete on the tables above |

The server never removes a row. A delete sets `deleted_at`, and `restore` clears it.

Every read goes through the views `live_floor`, `live_space` and `live_object`, which are the one definition of live: a row that is not deleted, on a site that is not deleted, and on a floor that is not deleted when it has one. `live_detail`, `live_detail_part` and `live_area` extend it: a detail is live when it, every detail it is inside, and its object, floor and site are live; an area when it and every area it is inside are live. Deleting a site or floor marks only that row, so every read tool stops returning what it holds at once, and restoring it brings all of it back as it was; `delete` reports how many floors, spaces and objects it hid. Writes use the tables. The views are granted to `house_qgis` and `house_ro` too, for QGIS layers that should show only what the server shows.

### The change log

Triggers on every model table write a `change` row for each insert, update and delete, whichever client made it: time, author, client (`application_name`), reason, operation, and the row before and after as `jsonb`. The server sets the author and reason for each transaction with `set_config(..., true)`, so they end with the transaction. A session logged in as any other role is recorded under that role, even if it sets an author of its own; any session may set `house.reason`. A row removed outright, from QGIS or `psql`, keeps its last state in `change`, which is what `restore` puts back.

### Roles

| Role | May |
| --- | --- |
| `house` | own everything; the server's role, and the only one that runs migrations |
| `house_qgis` | select, insert, update and delete on `site`, `floor`, `space`, `object`, `note`, `area`, `detail` and `detail_part`; select on everything else; never write `change` |
| `house_ro` | select on everything |

The migrations grant to `house_qgis` and `house_ro` but do not create them, because the owning role cannot create roles. Both must exist before the server first starts, and the database needs the `postgis` extension, which only a superuser can create. The migrations create `unaccent` themselves: it is a trusted extension, which the owning role may create.

## Tools

Every tool's description in `tools/list` says what it does, what it takes and what it returns, with an example where the arguments are not obvious. The tables below are a map.

Read tools, available to both tokens:

| Tool | Does |
| --- | --- |
| `list_sites` | live sites with their attributes, floors (with theirs), georeference and the area each is in |
| `get_floor` | a floor with its attributes, spaces and objects as GeoJSON |
| `find` | spaces, objects and areas by any name they go by, kind, outside identifier or attribute, best match first; see [Finding things](#finding-things) |
| `what_is_at` | the spaces containing a point, and the placed objects within a radius; with `floor_id`, outdoors and that floor, without it outdoors and every floor, each hit naming its floor |
| `in_space` | the placed objects and smaller spaces inside a space |
| `nearest` | the n placed objects of a kind nearest a point, space or object, in plan |
| `measure` | area, perimeter and length, and the distance between two things |
| `connections` | the openings that touch a space and the space beyond each, or the shortest way between two spaces through openings |
| `list_files` | files, optionally by what they are linked to |
| `get_file` | a file's metadata, with its content as text, as an image, or as one PDF page drawn to PNG; see [Viewing files](#viewing-files) |
| `get_detail` | a detail with its parts, measurements, placement, files, the details inside it and its path up to its object |
| `context` | from any area, site, floor, space, object or detail: the chain up to the outermost area and the direct children; see [Levels](#levels) |
| `search_notes` | notes containing some text |
| `history` | the `change` rows for one thing |
| `render_plan` | a floor or the site's outdoor areas as a PNG with a scale bar and north arrow, a detail in millimetres, or an area in UTM 32N, and a short JSON summary; the SVG too with `include_svg: true` |

Write tools take a required `reason`. They are refused to the read only token and left out of its tool list.

| Tool | Does |
| --- | --- |
| `create_site` | a site, optionally with an origin in any EPSG system, a rotation and an area |
| `set_site_origin` | set or replace a site's origin, in any EPSG system, and optionally its rotation |
| `set_site_area` | set or clear the area a site is in |
| `upsert_area` | create an area, or update one by `id`, keeping every field left out |
| `upsert_detail`, `upsert_detail_part` | create a detail or a part, or update one by `id`, keeping every field left out |
| `upsert_floor` | create a floor, or replace an existing floor's fields |
| `upsert_space`, `upsert_object` | create a row, or update one by `id`, keeping every field left out |
| `set_attributes` | add, change or remove some attributes of a site, floor, space or object |
| `link_ref` | set or remove one outside identifier on an object, such as `ha_entity_id` |
| `add_note` | a markdown note with its source, optionally linked to a detail or an area too |
| `upload_file` | text inline, or binary as base64 up to 25 MiB, optionally linked to a detail or an area too |
| `import_geojson` | features in any EPSG system as spaces or objects, converted into the site frame, or as areas in WGS 84 |
| `import_dxf` | a stored DXF drawing onto a floor, onto the site with no floor, or onto a detail in millimetres |
| `delete`, `restore` | mark a row deleted, or bring it back; a site, floor, detail or area hides or brings back all it holds |

### Finding things

`find` with `name` matches part of any name a thing goes by: the stored `name`, the attributes `common_name`, `name_en`, `label` and `nickname`, every string in a `nicknames` list, and every outside identifier in `refs`, such as `ha_name`. Case and accents do not matter, and a leading `the `, `il ` or `la ` is ignored on either side, so `snug` finds a room whose `common_name` is `the snug` and `citta` finds `Città`. Hits are ranked: an exact match on any field, then one that starts with the query, then one that contains it, with ties by name, and `limit` applies after ranking. Each hit names the field that matched in `matched_on`.

`find`, `in_space` and `nearest` return short hits unless asked for `detail: true`: `id`, `kind`, `name`, `site_id`, `floor_id`, `area` for a space, and `placement` and `in_space` for an object. `in_space` is the smallest live space on the object's floor that covers its position (the midpoint of a line, the centroid of anything else). A short `in_space` reply marks a line or area that crosses the space's edge with `partly_outside: true`. An object hit carries `details`, the number of detail drawings of it, and `files`, the number of files linked to it, each only when not zero, so a calling agent knows there is more to look at. `detail: true` asks for the full reply, which is about the reply's length and not about detail drawings: a hit also carries its attributes, outside identifiers and geometry, an object's detail drawings are listed as `details` (`id`, `name`, `view`), and `in_space` gives `wholly_inside` for every object. Without `site_id`, `find` also matches areas by the same names and lists them as `areas`. `what_is_at` and `connections` never return geometry. Every JSON reply is sent without indentation.

### Placement

An object's `placement` says how its geometry was found: `exact`, where it is; `approximate`, near where it is, such as the middle of its room; or `unplaced`, not known at all, with a stand in geometry because `geom` may not be null. `upsert_object` takes `placement`, `exact` by default on create. On update a `placement` left out keeps the stored one, even when new geometry is given, so moving an unplaced object to where it really is takes `"placement": "exact"` too.

`nearest`, `what_is_at` and `in_space` leave unplaced objects out and count them in `unplaced_omitted`, and every object they return carries `placement`; `nearest` ranks an approximate object like any other, and a distance to it is only as good as its room. `measure` and `nearest` refuse to measure from an unplaced object. `render_plan` does not draw unplaced objects and lists them in the legend as "N items not placed"; it draws approximate ones hollow, keyed in the legend as "position approximate". `find` returns unplaced objects like any other, so they can be found and placed.

Migration 0004 filled `placement` from the `position` attribute that seeded objects carried: `site_unplaced` became `unplaced`, and any other value, such as `room_centre`, `approximate`. The attribute was left as it was, and the migration logs how many objects had each value.

### Levels

The model zooms both ways from site, floor, space and object, and every level is optional.

Below an object, a **detail** is a drawing of it, such as a table seen from the top, or of part of another detail, such as a drawer inside that table, nested to any depth. A detail has its own frame: millimetres, x right, y up, origin at the drawing's lower left, with no relation to the site frame and no georeference. It carries labelled parts (`upsert_detail_part`), linework (`import_dxf` with `detail_id`, converted to millimetres from the drawing's unit, with layers mapped to `part` becoming parts), optional `measurements` (`width`, `depth` and `height`, each with its `source`: `measured`, `from_drawing` or `estimated`), and files and notes. A detail's optional `placement` (`x`, `y` in its parent's frame and units, `rotation`, `scale`) says where it lies in its parent; without one it is linked to the parent but not drawn in it. No floor or site query reads a detail: `what_is_at`, `nearest`, `in_space`, `measure`, `connections`, `get_floor` and floor and site plans answer as if there were none. `find`, `in_space` and `nearest` only count an object's details.

Above a site, an **area** is a neighbourhood, hamlet, town, municipality, valley or region (any lowercase `kind`), optionally inside another area, nested to any depth, with an optional outline stored in WGS 84 as given. A site is in the area its `area_id` names (`create_site`, `set_site_area`), or, with no `area_id`, in the smallest live area whose outline covers its origin. `list_sites` and `context` report which rule applied, as `via`: `area_id` or `origin`. `import_geojson` with `as: area` brings a boundary from a geoportal in directly.

`context` takes one of `area_id`, `site_id`, `floor_id`, `space_id`, `object_id` or `detail_id` and returns `up`, the chain from that row to the outermost area (a detail goes through the details it is inside, its object, the smallest space covering the object, the floor, the site and the areas), and `down`, the direct children by kind with counts. Each step says whether it is `placed` in its parent: a detail with a placement, an object that is not `unplaced`, a georeferenced site, an area with an outline.

`render_plan` draws each level. With `detail_id`, a detail's linework and labelled parts, its placed child details as dashed outlines, dimension lines from its measurements (an estimated value is marked `~`), and a scale bar in millimetres or centimetres, with no north arrow. With `area_id`, an area's outline and those of the areas directly inside it, and each georeferenced site in it as its parcel and building outlines carried out of the site frame into UTM zone 32N, labelled, with a scale bar and north arrow; a site with no georeference is listed in the legend. Floor and site plans are unchanged.

Deleting a detail hides its parts, linework and the details inside it, and deleting an area hides the areas inside it, through the live views; nothing cascades, and restoring brings it all back.

### Viewing files

`get_file` says in `content_kind` how it returns a file. `text`: UTF-8 text of at most 1 MiB, in `content`. `image`: a PNG, JPEG or WebP, as MCP image content no larger than 2000 pixels on its long side, scaled down when larger. `pdf_page`: one page of a PDF (`page`, from 1, default 1), drawn to a PNG about 2000 pixels on its long side, with `page_count`. PDF pages are drawn by [hayro](https://github.com/LaurenzV/hayro), a PDF rasterizer written in Rust, so the image needs no system library. `none`: any other file, or one that cannot be read, with `content` null and a `content_note` saying why when there is a reason to give.

### Updating part of a row

`upsert_space` and `upsert_object` need `name`, `kind` and `geometry` to create a row, and an error names the first one missing. With `id`, every field but `site_id` and `reason` may be left out and keeps its stored value, so a rename does not resend the geometry:

```json
{"id": 41, "site_id": 1, "name": "Reading lamp", "reason": "what we call it"}
```

Sending `floor_id`, `height` or `wall_material` as `null` clears it; leaving it out keeps it. `attributes` and `refs`, when given, replace them all; `set_attributes` and `link_ref` change some.

### Georeference

`create_site` may set the origin, and `set_site_origin` sets or replaces it later, with the rotation when given. Stored geometry is in the site frame and does not move; the origin says where that frame lies on the earth, which `import_geojson` uses to convert. The change is recorded with its reason like any other write.

```json
{"site_id": 3, "origin": {"epsg": 4326, "x": 9.19, "y": 45.46}, "rotation": 12.5, "reason": "origin at the south west corner of the survey"}
```

### DXF import

`import_dxf` reads LINE, LWPOLYLINE, POLYLINE, ARC, CIRCLE, INSERT (blocks expanded), TEXT and MTEXT from model space. Every entity read is kept as linework. A layer mapping such as `{"A-ROOM": {"space": "room"}, "E-LIGHT": {"object": "light"}}` turns closed polylines on a space layer into spaces, each named by the tallest text that PostGIS finds inside it, and block insertions on an object layer into objects at their insertion points. Layer names match in any case.

With `floor_id` the drawing goes onto that floor. With `site_id` and no `floor_id` it goes onto the site itself, as for a drawing of the grounds: its linework is drawn by `render_plan` on the site's outdoor plan, and the spaces and objects it makes are outdoor ones.

Units come from the drawing's `$INSUNITS` header. A drawing with no unit there is refused unless the call gives `unit`, which also overrides the header when both are present. An optional `transform` scales, rotates (degrees counterclockwise) and offsets the drawing after the unit conversion. The result reports counts per layer, the spaces and objects made, outlines PostGIS found invalid, and every entity skipped with the reason.

With `detail_id` alone it goes onto a detail: the drawing is converted to millimetres from its unit, its linework is kept for that detail only, and closed polylines on a layer mapped to `part`, such as `{"PARTS": {"part": "panel"}}`, become the detail's parts, each named by the tallest text inside it or by the mapped word and a number. A `transform` offset on a detail is in millimetres.

Importing the same file content onto the same floor, onto the same site with no floor, or onto the same detail, again marks the earlier import and everything it made as deleted, then imports afresh. A DWG file can be stored but not imported; convert it to DXF with the free ODA File Converter first.

### Plan labels

`render_plan` labels each space and object from the first attribute in `label_from` that holds a non-empty string, and otherwise from the row's `name` exactly as stored. `label_from` defaults to `["common_name", "name_en"]`: the family's nickname first, then the English name. A label taken from an attribute loses a leading "the " and starts with a capital, so a `common_name` of `the snug` is drawn as `Snug`. An empty list labels everything by `name`:

```json
{"site_id": 1, "floor_id": 2, "label_from": []}
```

Nicknames are ordinary attributes, so `set_attributes` changes a label on the next render:

```json
{"kind": "space", "id": 7, "set": {"common_name": "the snug"}, "reason": "what we call it"}
```

Labels are placed after the whole plan is laid out, so that none overlaps another, none covers an object's symbol or the north arrow, and none is cut short. Highlighted features go first, then spaces from the largest to the smallest, then objects. Each label tries its anchor (the middle of a space, just right of an object), then eight spots one text height around it, then the same at 80% and 65% of its size, and takes the first that is clear and inside the image; a space's label must still cover part of the space. A label with nowhere to go becomes a small numbered marker by its feature, with a leader line if it had to move, and its text is listed against the number in a legend in the footer beside the scale bar. The summary `render_plan` returns lists the legend. A legend too long for the image makes the image taller rather than lose an entry.

`label_size` is `small`, `normal` (the default) or `large`: 0.8, 1 and 1.35 times the usual type, for a phone or for print.

### Openings

An object of kind `opening` is a door, arch or open doorway. Its geometry is a LineString of exactly two points along the wall's centre line, from one jamb to the other, on the opening's floor. When `render_plan` draws that floor it removes the floor's linework within a flat ended buffer of the line, half of `wall_depth` to each side, and draws a jamb tick across the wall at each end, then the opening's symbol:

- an `arch` gets two dashed lines straight across the gap, one along each face of the wall, as a plan shows an opening with structure overhead;
- a `door` gets its leaf, a solid line square to the wall from the hinge jamb, and its swing, a thin quarter circle from the leaf's tip back to the other jamb; a pair of doors gets a half leaf and swing from each jamb;
- an `opening` gets the gap alone.

The stored linework is never changed, so moving or deleting the opening shows on the next render.

| Attribute | Value | Default |
| --- | --- | --- |
| `opening_type` | `door`, `arch` or `opening` | `opening` |
| `wall_depth` | the wall's thickness in metres, a positive number | 0.6 |
| `hinge` | for a door, the `start` or `end` of the line the leaf hangs from | `start` |
| `swing` | for a door, the `left` or `right` of the line's direction it opens towards | `left` |
| `leaves` | for a door, 1 or 2 | 1 |
| `connects` | the two space ids the opening joins | found from the geometry |

Any other geometry, or a LineString of more than two points, is drawn as an ordinary object and logged as a warning.

```json
{"site_id": 1, "floor_id": 2, "kind": "opening", "name": "Door to the hall",
 "geometry": {"type": "LineString", "coordinates": [[4.0, 1.2], [4.0, 2.1]]},
 "attributes": {"opening_type": "door", "hinge": "end", "swing": "right", "wall_depth": 0.3}, "reason": "door on the survey"}
```

```json
{"site_id": 1, "floor_id": 2, "kind": "opening", "name": "Arch to the kitchen",
 "geometry": {"type": "LineString", "coordinates": [[7.0, 0.5], [7.0, 2.1]]},
 "attributes": {"opening_type": "arch", "wall_depth": 0.45}, "reason": "arch on the survey"}
```

### Connections

`connections` answers how the spaces join. An opening joins the two spaces its `connects` attribute lists. Without that, it joins the spaces either side of it on its floor that reach within `wall_depth` of its midpoint, the smallest on each side, never a building or parcel outline. A `connects` that does not name two different live spaces of the site is reported in `warnings` and the geometry is used instead.

```json
{"space_id": 12}
{"from_space": 12, "to_space": 15}
```

The first lists every opening touching the space with its `opening_type` and the space on the other side, or null where nothing recorded is there, such as outdoors. The second returns the way passing the fewest openings, as an alternating list of spaces and openings; when there is none through recorded openings, `path` is null and `message` says so.

### Limits

| What | Limit |
| --- | --- |
| MCP request body | 36 MiB |
| file through a tool call | 25 MiB |
| request body through HTTP, which for an upload is the file | 100 MiB plus 64 KiB |
| stored file an import reads (DXF or GeoJSON) | 16 MiB |
| stored image or PDF `get_file` shows | 64 MiB; an image larger than 30,000 pixels on a side is not decoded |
| DXF entities, counting every copy a block expands to | 100,000 |
| DXF vertices, counted the same way | 2,000,000 |
| DXF block nesting | 16 levels; a block that inserts itself is refused |
| vertices in one geometry | 10,000 |
| GeoJSON features in one import | 10,000 |
| render size | 200 to 4096 pixels on the long side |

## HTTP

| Route | Auth | Does |
| --- | --- | --- |
| `GET /healthz` | none | 200 when the database answers; 503 while it does not, and while the server is still connecting at start |
| `POST /mcp` | bearer | MCP over streamable HTTP, stateless |
| `POST /api/files?name=...` | bearer, read and write | stores the request body; `Content-Type` is the media type; optional `reason`, `site_id`, `floor_id`, `space_id`, `object_id`, `detail_id`, `area_id` |
| `GET /api/files/<sha256>` | bearer | the file, as an attachment |
| `GET /` and `POST /` | basic, with the read and write token as the password | a page listing files with an upload form, for a phone |
| `GET /files/<sha256>` | basic, as above | the file, as an attachment |

Files are stored on the volume under their sha256 and are only ever addressed by it. Downloads are served as `application/octet-stream` attachments with `nosniff`. The page escapes every name, sends a strict Content Security Policy and refuses a form post whose `Origin` is not its own.

### Starting

The server listens at once and connects to the database behind the listener, trying again with a wait that doubles from 1 second up to 30, for up to 5 minutes, and logging each attempt at info. Until it has connected and run the migrations, every request, `/healthz` included, is answered 503, so a database that starts after the server leaves the pod not ready rather than restarting it. Only a database still out of reach after 5 minutes makes the process exit.

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

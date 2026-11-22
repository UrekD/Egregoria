# Lesce OSM import

Run `make osm-lesce` to build the FreeBuild slot. Make stores the bounded extract
in `EGR_OSM_CACHE/lesce-20261122-wood.overpass.json` (default `target/osm-cache`).
The Overpass query pins the historical data date because the scenario uses
specific Lesce road and railway anchors. The public endpoint must support
attic data. To reuse an existing extract, set `EGR_OSM_INPUT=/path/to/extract.json`.
Use `EGR_OSM_SAVE_ROOT` for the output directory and `EGR_OSM_FLAGS=--check`
to run the additional three-day headless verification. The default refuses to
overwrite the slot. Use `EGR_OSM_FLAGS=--replace` for an intentional replacement.

The report records the SHA-256 of the bytes actually consumed, the server's
`timestamp_osm_base`, and the preset query and requested historical date. An
explicit input override can contain data from a different query. The fetch
helper needs curl and jq. The generator uses sha256sum from GNU coreutils.
Keep the revision-associated report and OpenStreetMap attribution alongside the
save when sharing it. Attribution also appears in its Load-menu name.

`scenarios::osm_city::read_overpass` validates bounded Overpass JSON.
`normalize` projects and clips geometry into an 8,192 m square local map from
the supplied bounds. These two stages work with a different study area.
`Simulation::from_imported_city` builds the source-independent network.
The `osm-city` executable command is the Lesce scenario preset. Other areas
need their own corridor anchors, services, zoning, target scale, and camera.

The opening slot contains occupied mapped housing, mapped employment,
synthetic employment near arterials, and commissioned utilities. Passenger
service is represented by a static Lesce-Bled station marker. Flat terrain and
grade-separated structures are drawn in plan view. Run `make osm-lesce-shots`
against the generated save root for the overview, zoning, and traffic captures.

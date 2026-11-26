# Lesce Town

Open **Lesce Town · © OpenStreetMap contributors** from **Load**. The Free Build save opens with an established town, freight rail and local buses. The Sava runs along the south and west of town. Two road bridges connect the town to the far bank.

Open **Land Tiles** to expand past the starting 6 × 6 tile rectangle. Select a dry tile that shares an edge with land you own to see its price and confirm the purchase. The A2 and mainline rail continue through protected corridors into the outer tiles.

The Windows `make build` archive includes the commissioned town in `egregoria/world/`. For a separate download, open `egregoria-<version>-lesce-town.zip` with the game version named in the archive. Copy `lesce_town.meta.json` first and then `lesce_town.zip` into the game's `world/` directory or the save directory you select with `--save-dir`. Use a fresh save directory if you already have a Lesce Town slot.

The build report records the source pins, snapshot revision and opening checks. The street and river layouts use data from OpenStreetMap. [© OpenStreetMap contributors](https://www.openstreetmap.org/copyright)

## Build Lesce Town

Run `cargo run --locked --release -p scenarios -- citymap-build --out DIR` to commission the wet release slot and run its operating gates. Use a fresh output directory.

Pass `--dry` for a layout-only diagnostic. It writes the dry road network as a loadable slot with a layout report and checks the reloaded network. It skips commissioning, the operating soak and release acceptance checks.

## Release checks

Run `cargo run --locked --release -p scenarios -- citymap-build --verify DIR` to load the wet slot and check its report, version, revision, tick and population. Run `EGR_CITYMAP_PUBLISHED=DIR make build` to bundle that publication in the Windows archive. Run `make citymap-shots EGR_CITYMAP_OUT=DIR` to capture the opening camera, Sava bridges and Land Tiles panel. The captures' `captures.json` records the opening tick, revision and PNG hashes.

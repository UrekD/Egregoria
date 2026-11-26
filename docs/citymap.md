# Lesce Town

Open **Lesce Town · © OpenStreetMap contributors** from **Load**. The Free Build save opens with an established town, freight rail and local buses. The Sava runs along the south and west of town. Two road bridges connect the town to the far bank.

Open **Land Tiles** to expand past the starting 6 × 6 tile rectangle. Select a dry tile that shares an edge with land you own to see its price and confirm the purchase. The A2 and mainline rail continue through protected corridors into the outer tiles.

Release `linux.zip` and `windows.zip` include Starter Town, Showcase City, Lesce OSM and the commissioned Lesce Town in `world/`. To install the cities separately, extract `cities.zip` into the matching game version's directory, or copy the contents of its `world/` directory to the save directory you select with `--save-dir`. Use a fresh save directory if you already have any of those slots. The local Windows `make build` archive puts the same slots in `egregoria/world/`.

The build report records the source pins, snapshot revision and opening checks. The street and river layouts use data from OpenStreetMap. [© OpenStreetMap contributors](https://www.openstreetmap.org/copyright)

## Build Lesce Town

Run `cargo run --locked --release -p scenarios -- citymap-build --out DIR` to commission the wet release slot and run its operating gates. Use a fresh output directory.

Pass `--dry` for a layout-only diagnostic. It writes the dry road network as a loadable slot with a layout report and checks the reloaded network. It skips commissioning, the operating soak and release acceptance checks.

## Release checks

Run `cargo run --locked --release -p scenarios -- citymap-build --verify DIR` to load the wet slot and check its report, version, revision, tick and population. Run `EGR_CITYMAP_PUBLISHED=DIR make build` to bundle that publication in the Windows archive. Run `make citymap-shots EGR_CITYMAP_OUT=DIR` to capture the opening camera, Sava bridges and Land Tiles panel. The captures' `captures.json` records the opening tick, revision and PNG hashes.

Release CI runs `scripts/build-windows-with-slots.sh --release-slots WORLD_DIR CARGO_TARGET_DIR` once on Linux. Use a new output directory for `WORLD_DIR`. The script decompresses the archived Overpass responses in `scripts/osm/release/`. Their exact bytes match the Lesce and Sava SHA-256 pins checked by `citymap-build`. OpenStreetMap contributors provide the archived data under the Open Database License. [OpenStreetMap attribution and license](https://www.openstreetmap.org/copyright)

For a local packaging run, set `EGR_CITYMAP_PUBLISHED` to reuse a verified wet town for the current save version.

## Trigger a release

Push one `v*` tag at a time from a commit containing `.github/workflows/release.yml`, for example `git push origin refs/tags/vX.Y.Z`. Push with user credentials or a GitHub App installation token if automated. GitHub suppresses workflow runs for pushes made with the repository's `GITHUB_TOKEN`, and pushes of more than three tags produce no tag events. [GitHub workflow triggers](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow) · [GitHub push events](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#push)

If a tag push produces no workflow run, use `gh workflow run release.yml --ref vX.Y.Z`. The workflow must also exist on the default branch for manual dispatch. Dispatching a branch produces workflow artifacts. Dispatching a tag publishes the GitHub release. [GitHub workflow dispatch](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_dispatch)

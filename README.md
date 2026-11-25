![Screenshot of Egregoria 1](assets/screen2.jpg)
![Screenshot of Egregoria 2](assets/screen3.jpg)
![Screenshot of Egregoria 3](assets/screen5.jpg)
![Screenshot of Egregoria 3](assets/screen1.jpg)

[![Build status](https://github.com/Uriopass/Egregoria/workflows/rust-build/badge.svg)](#)
[![Discord](https://img.shields.io/discord/709730057949544488?label=discord)](https://discord.gg/CAaZhUJ)

# Egregoria

Egregoria is an indie city builder, mostly inspired by Cities:Skylines.  
Each individual has its own thought model, meaning every action has its importance and influences the environment.  
Egregoria is focused on the socio-economical aspect of a city, with a logistics element.
The game is still in early development, but you can already play it and give some feedback through
[issues](https://github.com/Uriopass/Egregoria/issues) or on [discord](https://discord.gg/CAaZhUJ).  

By being open source, the hope is to get more people involved in the development of the game.  
Mod support is wanted but the design has not been found yet.

## Fork development progress (placeholder visuals)

This fork is working toward Cities: Skylines-style core gameplay (zoning, demand, growth, budget, services, transit) with placeholder visuals first; models and art come after the gameplay has been playtested.

### Milestone P0 — verification foundation

Headless scenario runner, determinism and save/resume checks, lifecycle cleanup, validated commands and scripted screenshots (Xvfb + software Vulkan).

| Overview | Economy window |
|---|---|
| ![P0 smoke overview](docs/screenshots/p0-smoke-overview.png) | ![P0 smoke economy](docs/screenshots/p0-smoke-economy.png) |

### Milestone P1 — zoning, demand, growth, budget

Residential/commercial/industrial/office zoning (low/high density), RCI demand with driver tooltips, automatic growth and level-ups, households with immigration/emigration, growable companies on the goods market, retail trips, taxes, upkeep and a daily budget ledger.

| Zoning | Grown city | Budget |
|---|---|---|
| ![P1 zoning](docs/screenshots/p1-zoning.png) | ![P1 grown city](docs/screenshots/p1-grown-city.png) | ![P1 budget](docs/screenshots/p1-budget.png) |

### Milestone P2 — services, utilities, environment, happiness

Health, fire, police, school, garbage and park services with road-network coverage and shared capacity; water and sewage on the road network; pollution and land value; per-household happiness feeding demand; L2/L3 upgrades gated by services; fires; service upkeep; map overlays, needs inspector and problem badges.

| Coverage overlay | Needs inspector | Services |
|---|---|---|
| ![P2 school coverage](docs/screenshots/p2-coverage-school.png) | ![P2 needs inspector](docs/screenshots/p2-needs-inspector.png) | ![P2 services](docs/screenshots/p2-services-tab.png) |

### Milestone P3 — public transit (buses)

Bus stops anchored to road geometry, looped lines with fleet and fare controls, buses dispatched on directed berths with dwell and FIFO boarding, citizens choosing walk/car/bus by estimated travel time (headway learned from completed runs), fares and bus upkeep in the budget, trip statistics by mode, transit overlay and line panel. Plus a performance pass: 10,000 commuting citizens run at ~290 simulation ticks/s on the test host.

| Transit overlay | Line panel |
|---|---|
| ![P3 transit overlay](docs/screenshots/p3-transit-overlay.png) | ![P3 transit lines](docs/screenshots/p3-transit-lines.png) |

### Milestone P4 — game shell and UX

Main menu with Continue/New Game/Load, new-game options (terrain preset, seed, map size, starting money), named save slots with atomic writes, Save As, rotating autosaves, Esc pause menu with unsaved-changes prompts, drag-to-bulldoze, tooltips, traffic congestion overlay, an advisor panel with alerts and "go to" links, and "why" diagnostics for buildings and citizens. A second performance pass brings 10,000 commuting citizens to ~440–530 simulation ticks/s on the test host.

| New game | Advisor | Traffic overlay |
|---|---|---|
| ![P4 new game](docs/screenshots/p4-new-game.png) | ![P4 advisor](docs/screenshots/p4-advisor.png) | ![P4 traffic](docs/screenshots/p4-traffic.png) |

### Milestone P5 — playtest feedback

- **Households keep a pantry.** Needs drive multi-unit grocery baskets bought at one reachable bakery or store, carried home and eaten over time. Citizens also go to work daily, visit parks and shop after work.
- **Freight moves on trucks.** Stock is tracked as on hand, reserved and in transit, and trucks leave with full 16-unit loads. The Small depot (64 units) and Large warehouse (256 units) buffer goods under Balanced, Reserve locally or Export surplus policies. Rail imports are physical: trains deliver to the freight station, station trucks carry the goods to the buyer (with a freight surcharge), and without a route there is no import. Zoned industry has its own trucks.
- **Guided mode.** New Game offers Guided or Free build. Guided cities reach milestones at 100, 250 and 500 citizens with one-time grants and closed-day objectives. Locked cards show the population they need, and the Progression window tracks the next milestone.
- **Zoning densities and plots.** All four zones have low, medium and high density in distinct shades. Zoning uses a 16 m stamp with 8 m detail and draws its grid while zoning. Plots are Varied (mixed 20/30/40 m) or Uniform, and lots never overlap.
- **Overlays painted on the land.** Service reach, pollution, estimated land value and utility networks are drawn on the terrain. Each service shows its served area and a max-reach ring, and placing a service turns on its overlay. Service radii are at city scale, for example a 600 m school and a 900 m fire station.
- **Placement and inspectors.** Placement uses real intersection and curb geometry, previews are validated like the command, and a red ghost marks a rejected spot. Services and warehouses are lettered placeholder boxes. The road inspector shows current users, entries in the last 10 game minutes, average speed against the limit (Free/Busy/Jammed) and each truck's cargo and destination.
- **Toolbar and time controls.** The toolbar uses Cities: Skylines-style categories with shelves (Roads holds Straight and Curved) and remembers each category's last mode. Speeds run 8×, 20×, 50× and MAX (key 6). The per-frame simulation budget grows from 25 ms up to 8× to 60 ms at 50× and MAX, and the clock shows the achieved speed when it falls short. Right click cancels the tool or selection.
- **Lesce at real scale.** The Lesce import opens with about 2.9k residents in 1.4k homes. It uses OpenStreetMap forests, priority signage at junctions, traffic lights where OSM maps signals and at arterial crossroads, and yielding roundabouts.
- **Performance.** `perf_10k` (10,000 residents) runs at about 230–245 ticks/s on the 16-thread test host. With the 3× clock each tick covers 0.6 game seconds, so the simulation advances game time about 2× faster than before P5 at the same CPU cost.
- **Faster calendar.** The game clock runs 30 game seconds per real second, so a day takes 48 real minutes at 1×. Vehicles still move at real speed.
- **Traffic fixes.** Cars with no driving route park and walk, cancelled drives park before the driver leaves, and carless errands may ride the bus.

| Zoning densities | School placement | Road inspector |
|---|---|---|
| ![P5 zoning densities](docs/screenshots/p5-zoning-densities.png) | ![P5 school placement](docs/screenshots/p5-school-placement.png) | ![P5 road inspector](docs/screenshots/p5-road-inspector-cargo.png) |

| Guided progression | Roads shelf | Lesce |
|---|---|---|
| ![P5 guided progression](docs/screenshots/p5-guided-progression.png) | ![P5 roads shelf](docs/screenshots/p5-toolbar-roads.png) | ![P5 Lesce overview](docs/screenshots/p5-lesce-overview.png) |

On Linux, `make p5-shots` regenerates the P5 screenshots under Xvfb from freshly generated demo and Lesce slots.

### Milestone P6 — water, zoning and city expansion

- **Water and bridges.** Rivers, lakes and seas shape where you can build. Use the water brush to edit rivers and lakes, and build road or rail bridges across water. Pumps need a river or lake shore, while sewage outlets need a river bank or coast. On rivers, outlets sit downstream of pumps. Shorelines raise nearby land value.
- **Zoning and building height.** Profiled zoning gives low-density houses, mid-rise blocks and high-density towers distinct shapes. Housing develops through level 3 when local needs are met. Paint Medium or High Mixed Use to grow homes above working shops at the same address.
- **Buyable land tiles.** Expand a tiled city Cities: Skylines-style by buying adjacent 512 m × 512 m tiles from the land map. Guided mode ties purchases to population milestones. The first tile costs $10,000 and each purchase raises the next price by $500.
- **Lesce Town.** The authored map uses an organic street network based on OpenStreetMap at 2× source scale. The dry layout is in place. The playable Sava river-town save, with a target of about 2,600 residents, is pending validation and publication.
- **Freight and food.** Fairer order matching and better truck haulage help stock reach buyers, with priority for fuel and other inputs that have run out. Food balancing improves shop supply, with about 94–99% of due grocery baskets filled in test towns.
- **Performance.** Skipping idle citizens improved benchmark throughput by 11%, and the single-player simulation runs on its own thread. Use `perf_scale` for scale benchmarks and `bench-slot` for saved cities. The 10,000-resident benchmark ran at 213 ticks/s, about 4× real time on the test host.
- **Replays and saves.** Replay checks verify that repeated runs produce identical simulation results. The snapshot and replay format moved to 0.10.0, with the 0.11.0 cut pending.

| Water and bridges | Zoning and towers | Land tiles | Lesce Town |
|---|---|---|---|
| [TODO: water and bridges screenshot](docs/screenshots/p6-water-bridges.png) | [TODO: zoning and towers screenshot](docs/screenshots/p6-zoning-towers.png) | [TODO: land tiles screenshot](docs/screenshots/p6-land-tiles.png) | [TODO: Lesce Town screenshot](docs/screenshots/p6-lesce-town.png) |

Developer commands (see `Makefile`): `make check`, `make test`, `make scenarios`, `make determinism`, `make perf`, `make shots`.

### Try the demo cities

Run `make demo-cities` from the repository root. The generator writes **Starter Town** and **Showcase City** to `world/`, replacing any saves with the same slot IDs. Open either city from **Load**, or use **Continue** for the most recent one.

- **Starter Town** has a small zoned grid, utilities, and a road to the rail freight terminal. Resume time to watch the first residents move in and staff the town.
- **Showcase City** has grown for over three game days. Try its staffed services, traffic and utility overlays, two bus loops, Budget, and City Stats.

For a separate save directory, run `cargo run --locked --release -p scenarios -- demo-cities --save-root <dir>` and launch with `cargo run --locked --release -p native_app -- --save-dir <dir>`. Camera position and time speed use the game's normal settings when you load. Press **Space** to pause if time is running, and pan toward the freight terminal near map coordinates `(4300, 6300)`. Both cities lie just west of it.

### Try Lesce, Slovenia

Run `make osm-lesce` and open **Lesce, Slovenia** from **Load**. The Free-build slot uses local OpenStreetMap road and rail geometry, occupied houses, and zoning from mapped buildings and land use. The import report identifies supplemental employment, utilities, freight and motorway turnarounds. Bridges and tunnels are rendered flat. Road boundary stubs are cosmetic, and outside trade uses freight rail. Lesce earns high income under the game's default tax rates, so the opening treasury grows quickly during free build.

The pinned extract opens with 2,903 residents in 1,366 homes, $250,000 in the treasury, and full utility service. The opening has two mapped fire stations and a supplemental station between Lesce and Radovljica.

The three-day check ends with 3,129 residents in 1,311 homes and full utility service at every four-hour sample. The treasury gains about $121,000–$126,000 per game day.

The default extract cache is `target/osm-cache/lesce-20261122-signals.overpass.json`. Set `EGR_OSM_CACHE` to choose another cache directory or `EGR_OSM_INPUT` to use a local Overpass response without fetching. The fetch uses `curl` and `jq`, and the generator uses `sha256sum`. Set `EGR_OSM_SAVE_ROOT` to change the default `world/` save root. Use `EGR_OSM_FLAGS=--check` to run the three-game-day growth, utilities, employment and freight checks before publication. Use `--replace` in `EGR_OSM_FLAGS` to explicitly replace an existing Lesce slot.

On Linux, run `make osm-lesce-shots EGR_OSM_SAVE_ROOT=<dir>` to capture the opening camera, zoning and traffic under Xvfb. The generated slot, import report and attribution are local artifacts.

Map data © OpenStreetMap contributors, available under the [Open Database License (ODbL)](https://www.openstreetmap.org/copyright).

## How to play

A small tutorial is available on the [github wiki]((https://github.com/Uriopass/Egregoria/wiki/Introduction-Guide)) to get you started.

## Devblog  

I keep a blog about Egregoria's development [here](http://douady.paris/blog/index.html).

## Building the project

### Git LFS

This project uses Git LFS to track assets, therefore if you want to build your own copy you will need to install [Git LFS](https://git-lfs.github.com/).

Once installed, you should be able to clone the repo and fetch the assets:

```bash
git clone https://github.com/Uriopass/Egregoria
cd Egregoria
git lfs pull
```

### Windows/Mac
I personally use Windows 10 and Mac OS 10.11, and it compiles fine once the [rust toolchain is installed](https://www.rust-lang.org/tools/install).
```bash
cargo run --release
```

Don't forget to add the `--release` flag, otherwise the game will be very slow.  
Don't forget to pull the lfs files, otherwise the game will crash with a file not found error.

### Ubuntu/Debian on x11
There are a few libraries to install that some of my dependencies need:

```
sudo apt-get install libasound2-dev libudev-dev pkg-config libx11-dev
cargo run --release
```

Don't forget to add the `--release` flag, otherwise the game will be very slow.  
Don't forget to pull the lfs files, otherwise the game will crash with a file not found error.

A GitHub Action tests the builds on Ubuntu.

## Why Egregoria ?

An Egregor is an autonomous psychic entity made up of, and influencing, the thoughts of a group of people.  
It represents emergence at its purest form, where individuals come together to become a collective force.

## Credits

- [`@dabreegster`](https://github.com/dabreegster): For inspiration on the map model
- PBR Shaders are adapted from [LearnOpenGL](https://learnopengl.com/PBR/Theory)

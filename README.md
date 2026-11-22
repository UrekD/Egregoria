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

Developer commands (see `Makefile`): `make check`, `make test`, `make scenarios`, `make determinism`, `make perf`, `make shots`.

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

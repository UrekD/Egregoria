use crate::builders::{grid_city, paint_zone_blocks, place_grid_power_plant, place_service_sites};
use crate::ScenarioCtx;
use geom::{vec2, Vec2};
use prototypes::{
    try_prototype, BuildingGen, GrowableCompanyID, Money, RollingStockID, Tick, TICKS_PER_HOUR,
};
use simulation::economy::{
    Budget, BudgetCategory, Government, TripStats, DEFAULT_TAX_RATE_PERMILLE, TICKS_PER_BUDGET_DAY,
};
use simulation::map::{
    BuildingID, BuildingKind, Density, LaneKind, LanePatternBuilder, PathKind, ProjectFilter,
    ProjectKind, RoadID, ZoneKind,
};
use simulation::map_dynamic::{ElectricityFlow, Itinerary, Transit, UtilityFlow};
use simulation::souls::residence::Residences;
use simulation::transportation::traffic_metrics;
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{SavePhase, Simulation, SimulationOptions, SlotId, SnapshotRevision};
use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

const OPTIONS: SimulationOptions = SimulationOptions {
    save_replay: false,
    starting_money_bucks: 350_000,
    ..SimulationOptions::LEGACY
};
const STARTER_ORIGIN: Vec2 = vec2(3630.0, 6120.0);
const SHOWCASE_ORIGIN: Vec2 = vec2(3070.0, 5520.0);
const SPACING: f32 = 100.0;
const FREIGHT_APPROACH: Vec2 = vec2(4190.0, 6320.0);
const SHOWCASE_DAYS: u64 = 3;
const BUSES_PER_LINE: u8 = 5;

use Density::{High, Low};
use ZoneKind::{Commercial, Industrial, Office, Residential};

/// Build both demo cities and publish them as named, replay-free save slots.
pub fn generate(root: &Path) -> Result<(), String> {
    let root = std::path::absolute(root).map_err(|error| error.to_string())?;
    let starter = build_starter()?;
    publish(&starter.sim, &root, "starter_town", "Starter Town")?;
    drop(starter);

    let showcase = build_showcase()?;
    publish(&showcase.sim, &root, "showcase_city", "Showcase City")
}

fn publish(sim: &Simulation, root: &Path, id: &str, name: &str) -> Result<(), String> {
    let id = SlotId::new(id).map_err(|error| error.to_string())?;
    // Give identical simulated states identical snapshot headers. The slot API
    // still records the current publication time in the metadata sidecar.
    let hashes = sim.hashes();
    let revision_half = |salt: u8| {
        let mut hasher = DefaultHasher::new();
        (salt, id.as_str(), &hashes).hash(&mut hasher);
        u128::from(hasher.finish())
    };
    let revision = SnapshotRevision::new(revision_half(0) << 64 | revision_half(1));
    let result = sim
        .encode_slot(revision)
        .and_then(|slot| slot.with_display_name(name.to_owned()).publish(root, &id));
    match result.map_err(|error| format!("{name}: {error}"))? {
        SavePhase::Complete => {
            println!("Wrote {name} ({id}) to {}", root.display());
            Ok(())
        }
        SavePhase::SidecarsFailed { replay, meta } => Err(format!(
            "{name}: snapshot published but sidecars failed: replay={replay:?}, metadata={meta:?}"
        )),
    }
}

fn build_starter() -> Result<ScenarioCtx, String> {
    let mut ctx = ScenarioCtx::new(OPTIONS);
    let roads = build_grid(&mut ctx, STARTER_ORIGIN, 4)?;
    connect_freight(&mut ctx, STARTER_ORIGIN + vec2(300.0, 200.0))?;
    place_grid_power_plant(&mut ctx, STARTER_ORIGIN)?;
    place_service_sites(
        &mut ctx,
        STARTER_ORIGIN,
        [
            ("coal-mine", vec2(275.0, 330.0)),
            ("water-pump", vec2(-25.0, 50.0)),
            ("sewage-outlet", vec2(-25.0, 250.0)),
        ],
    )?;
    let blocks = (0..3).flat_map(|y| {
        (0..3).map(move |x| {
            let kind = match (x, y) {
                (2, 2) => Industrial,
                (2, _) => Commercial,
                _ => Residential,
            };
            (x, y, kind, Low)
        })
    });
    paint_zone_blocks(&mut ctx, STARTER_ORIGIN, SPACING, blocks);
    check_commands(&ctx.sim)?;
    let money = ctx.sim.read::<Government>().money;
    if money < Money::new_bucks(100_000) {
        return Err(format!("Starter Town has only {money} after construction"));
    }
    println!(
        "Starter Town: {} grid roads, low-density R/C/I, rail freight, treasury {money}",
        roads.len()
    );
    Ok(ctx)
}

fn build_showcase() -> Result<ScenarioCtx, String> {
    let mut ctx = ScenarioCtx::new(OPTIONS);
    let start = ctx.sim.get_tick();
    let roads = build_grid(&mut ctx, SHOWCASE_ORIGIN, 9)?;
    let freight = connect_freight(&mut ctx, SHOWCASE_ORIGIN + Vec2::splat(800.0))?;
    let plant = place_grid_power_plant(&mut ctx, SHOWCASE_ORIGIN)?;
    let sites = place_service_sites(
        &mut ctx,
        SHOWCASE_ORIGIN,
        [
            ("coal-mine", vec2(780.0, 830.0)),
            ("water-pump", vec2(-30.0, 50.0)),
            ("sewage-outlet", vec2(-30.0, 750.0)),
            ("school", vec2(225.0, 375.0)),
            ("clinic", vec2(325.0, 225.0)),
            ("fire-station", vec2(125.0, 250.0)),
            ("police-station", vec2(525.0, 375.0)),
            ("garbage-depot", vec2(725.0, 550.0)),
            ("park", vec2(175.0, 350.0)),
            ("park", vec2(325.0, 650.0)),
        ],
    )?;

    let blocks = (0..8).flat_map(|y| {
        (0..8).map(move |x| {
            let (kind, density) = match (x, y) {
                (2, 2 | 5) | (4, 1 | 6) => (Commercial, Low),
                (0, _) | (1 | 2, 0) => (Residential, Low),
                (1..=4, _) => (Residential, High),
                (5, 0 | 3 | 6) => (Commercial, Low),
                (5, _) => (Commercial, High),
                (6, 0 | 2 | 4 | 6) => (Office, Low),
                (6, _) => (Office, High),
                (7, 0..=3) => (Industrial, High),
                _ => (Industrial, Low),
            };
            (x, y, kind, density)
        })
    });
    paint_zone_blocks(&mut ctx, SHOWCASE_ORIGIN, SPACING, blocks);
    check_commands(&ctx.sim)?;

    add_bus_loop(
        &mut ctx.sim,
        &[
            ((1, 1), (2, 1)),
            ((3, 1), (4, 1)),
            ((5, 1), (6, 1)),
            ((7, 2), (7, 3)),
            ((7, 4), (6, 4)),
            ((5, 4), (4, 4)),
            ((3, 4), (2, 4)),
            ((1, 3), (1, 2)),
        ],
    )?;
    add_bus_loop(
        &mut ctx.sim,
        &[
            ((0, 4), (1, 4)),
            ((2, 4), (3, 4)),
            ((4, 4), (5, 4)),
            ((6, 5), (6, 6)),
            ((6, 7), (5, 7)),
            ((4, 7), (3, 7)),
            ((2, 7), (1, 7)),
            ((0, 6), (0, 5)),
        ],
    )?;

    // Office demand depends on unemployment. A short tax break gives both
    // densities a chance to grow before the city's other employers fill up.
    ctx.sim.apply_commands([&WorldCommand::SetTaxRate {
        zone: Office,
        rate_permille: 0,
    }]);
    check_commands(&ctx.sim)?;
    ctx.run_ticks(2 * TICKS_PER_HOUR);
    ctx.sim.apply_commands([&WorldCommand::SetTaxRate {
        zone: Office,
        rate_permille: DEFAULT_TAX_RATE_PERMILLE,
    }]);
    check_commands(&ctx.sim)?;

    for day in 1..=SHOWCASE_DAYS {
        let target = start + day * TICKS_PER_BUDGET_DAY;
        ctx.run_ticks(target - ctx.sim.get_tick());
        println!(
            "Showcase City: day {day} complete, {} residents, {} completed bus trips",
            ctx.sim.world().humans.len(),
            ctx.sim.read::<TripStats>().transit.completed,
        );
    }
    // Save during the following afternoon's commute so the traffic overlay has
    // vehicle samples directly after loading the snapshot.
    ctx.run_ticks(8 * TICKS_PER_HOUR);
    super::services_basic::check_service_network(&ctx, plant, &sites, "Showcase City")?;
    check_showcase(&ctx.sim, plant, freight, &roads)?;
    Ok(ctx)
}

fn build_grid(
    ctx: &mut ScenarioCtx,
    origin: Vec2,
    intersections: u32,
) -> Result<Vec<RoadID>, String> {
    let grid = grid_city(ctx, origin, intersections, intersections, SPACING);
    let expected = (2 * intersections * (intersections - 1)) as usize;
    if grid.roads.len() != expected {
        return Err(format!(
            "built {} of {expected} grid roads",
            grid.roads.len()
        ));
    }
    Ok(grid.roads)
}

fn connect_freight(ctx: &mut ScenarioCtx, grid_point: Vec2) -> Result<BuildingID, String> {
    let (command, station) = {
        let map = ctx.sim.map();
        let point = |xy: Vec2| {
            let height = map.environment.height(xy).unwrap_or_default();
            map.project(xy.z(height), 0.0, ProjectFilter::INTER)
        };
        let station = map
            .buildings()
            .values()
            .find(|building| matches!(building.kind, BuildingKind::RailFreightStation(_)))
            .ok_or("map is missing the startup freight terminal")?;
        let command = WorldCommand::MapMakeConnection {
            from: point(grid_point),
            to: point(FREIGHT_APPROACH),
            inter: None,
            pat: LanePatternBuilder::new().build(),
        };
        (command, (station.id, station.obb, station.kind))
    };
    ctx.sim.apply_commands([&command]);
    check_commands(&ctx.sim)?;
    let road = {
        let map = ctx.sim.map();
        let center = (grid_point + FREIGHT_APPROACH) * 0.5;
        let height = map.environment.height(center).unwrap_or_default();
        let ProjectKind::Road(road) = map.project(center.z(height), 0.0, ProjectFilter::ROAD).kind
        else {
            return Err("freight approach road is missing".into());
        };
        road
    };

    // Preserve the terminal's original platform and rail access when replacing
    // its roadless bootstrap building with a driveway-connected one.
    let (old_id, pos, kind) = station;
    ctx.sim.apply_commands([
        &WorldCommand::MapRemoveBuilding(old_id),
        &WorldCommand::MapBuildSpecialBuilding {
            pos,
            kind,
            gen: BuildingGen::NoWalkway {
                door_pos: Vec2::ZERO,
            },
            zone: None,
            connected_road: Some(road),
        },
    ]);
    check_commands(&ctx.sim)?;
    let map = ctx.sim.map();
    let terminal = map
        .buildings()
        .values()
        .find(|building| building.kind == kind)
        .filter(|building| building.connected_road == Some(road))
        .ok_or("freight terminal has no road access")?;
    let outside = map
        .buildings()
        .values()
        .find(|building| building.kind == BuildingKind::ExternalTrading)
        .ok_or("map is missing the external trading building")?;
    let tick = Tick(ctx.sim.get_tick());
    for (kind, src, dst) in [
        (
            PathKind::Rail,
            terminal.obb.center().z0(),
            outside.obb.center().z0(),
        ),
        (PathKind::Vehicle, grid_point.z0(), terminal.door_pos),
    ] {
        if Itinerary::route(tick, src, dst, &map, kind).is_none()
            || Itinerary::route(tick, dst, src, &map, kind).is_none()
        {
            return Err(format!(
                "freight terminal has no bidirectional {kind:?} route"
            ));
        }
    }
    let terminal = terminal.id;
    let rail = map
        .nearest_lane(outside.obb.center().z0(), LaneKind::Rail, None)
        .and_then(|id| map.get(id))
        .ok_or("outside trading has no freight train starting lane")?;
    let train = WorldCommand::SpawnTrain {
        wagons: std::iter::once(RollingStockID::new("locomotive"))
            .chain([RollingStockID::new("freight-wagon"); 3])
            .collect(),
        lane: rail.id,
        dist: rail.points.length() * 0.5,
    };
    drop(map);
    // The legacy startup train command is a no-op. Dispatch needs a real consist.
    ctx.sim.apply_commands([&train]);
    check_commands(&ctx.sim)?;
    Ok(terminal)
}

type GridPoint = (u32, u32);

fn add_bus_loop(sim: &mut Simulation, legs: &[(GridPoint, GridPoint)]) -> Result<(), String> {
    let commands = {
        let map = sim.map();
        let junction = |(x, y): GridPoint| {
            let pos = SHOWCASE_ORIGIN + vec2(x as f32, y as f32) * SPACING;
            map.intersections()
                .values()
                .find(|node| node.pos.xy().is_close(pos, 1.0))
                .map(|node| node.id)
        };
        legs.iter()
            .map(|&(from, to)| {
                let lane = junction(from)
                    .zip(junction(to))
                    .and_then(|(src, dst)| {
                        let road = map
                            .find_road(src, dst)
                            .or_else(|| map.find_road(dst, src))?;
                        map.roads()[road].lanes_iter().find_map(|(id, kind)| {
                            let lane = &map.lanes()[id];
                            (kind == LaneKind::Driving && lane.src == src && lane.dst == dst)
                                .then_some(lane)
                        })
                    })
                    .ok_or_else(|| format!("missing bus lane {from:?} to {to:?}"))?;
                let (pos, dir) = lane.points.point_dir_along(lane.points.length() * 0.5);
                Ok(WorldCommand::TransitAddStop { pos, dir: dir.xy() })
            })
            .collect::<Result<Vec<_>, String>>()?
    };

    let mut stops = Vec::with_capacity(commands.len());
    for command in commands {
        let count = sim.read::<Transit>().stops.len();
        sim.apply_commands([&command]);
        check_commands(sim)?;
        let transit = sim.read::<Transit>();
        let stop = transit
            .stops
            .keys()
            .last()
            .filter(|_| transit.stops.len() == count + 1)
            .ok_or("transit command did not add a stop")?;
        stops.push(stop);
    }
    sim.apply_commands([&WorldCommand::TransitCreateLine { stops }]);
    check_commands(sim)?;
    let line = sim
        .read::<Transit>()
        .lines
        .keys()
        .last()
        .ok_or("bus loop missing")?;
    sim.apply_commands([&WorldCommand::TransitSetVehicles {
        line,
        n: BUSES_PER_LINE,
    }]);
    check_commands(sim)
}

fn check_commands(sim: &Simulation) -> Result<(), String> {
    let outcomes = sim.read::<CommandOutcomes>();
    if !outcomes.last.is_empty() {
        return Err(format!("demo construction rejected: {:?}", outcomes.last));
    }
    Ok(())
}

fn check_showcase(
    sim: &Simulation,
    plant: BuildingID,
    freight: BuildingID,
    roads: &[RoadID],
) -> Result<(), String> {
    let population = sim.world().humans.len();
    let unemployed = sim
        .world()
        .humans
        .values()
        .filter(|human| human.work.is_none())
        .count();
    let unemployment = 100.0 * unemployed as f64 / population.max(1) as f64;
    let residences = sim.read::<Residences>();
    let zone_types = [Residential, Commercial, Industrial, Office];
    let developed: BTreeSet<_> = sim
        .world()
        .companies
        .values()
        .filter_map(|company| try_prototype(GrowableCompanyID::from(company.comp.proto)))
        .filter(|proto| proto.growable)
        .map(|proto| (proto.zone_kind, proto.density))
        .chain(
            residences
                .by_building
                .values()
                .map(|home| (Residential, home.density)),
        )
        .collect();
    for zone in zone_types {
        for density in [Low, High] {
            if !developed.contains(&(zone, density)) {
                return Err(format!(
                    "Showcase City has no developed {density:?} {zone:?} lots"
                ));
            }
        }
    }
    let l2 = residences
        .by_building
        .values()
        .filter(|home| home.level == 2)
        .count();
    let l3 = residences
        .by_building
        .values()
        .filter(|home| home.level == 3)
        .count();
    let map = sim.map();
    let net = map
        .electricity
        .net_id(plant)
        .ok_or("Showcase City power plant is disconnected")?;
    let electricity = sim.read::<ElectricityFlow>();
    let power = electricity.network_stats(net);
    let utilities = sim.read::<UtilityFlow>();
    let water = utilities
        .networks()
        .find_map(|(id, totals)| (id == net).then_some(totals))
        .ok_or("Showcase City has no utility flow")?;
    let unserved = map
        .buildings()
        .values()
        .filter(|building| building.kind != BuildingKind::ExternalTrading)
        .filter(|building| {
            map.electricity.net_id(building.id) != Some(net)
                || electricity.blackout(net)
                || !utilities.has_water(building.id)
                || !utilities.has_sewage(building.id)
        })
        .count();
    drop(map);

    let transit = sim.read::<Transit>();
    let both_loops_running = transit.lines.len() == 2
        && transit.lines.iter().all(|(line, info)| {
            !info.suspended && transit.runs.values().any(|run| run.line == line)
        });
    let bus_trips = sim.read::<TripStats>().transit.completed;
    let traffic_roads = traffic_metrics::sample(sim, roads.iter().copied()).len();
    let treasury = sim.read::<Government>().money;
    let budget = sim.read::<Budget>();
    if zone_types
        .into_iter()
        .any(|zone| budget.tax_rate(zone) != DEFAULT_TAX_RATE_PERMILLE)
    {
        return Err("Showcase City must use default tax rates".into());
    }
    let daily_balance = budget
        .days()
        .get(&(SHOWCASE_DAYS - 1))
        .map(|day| day.values().sum::<i128>())
        .unwrap_or_default();
    let balance = Money::new_inner(daily_balance as i64);
    let imports = budget.total(BudgetCategory::TradeImport);
    let exports = budget.total(BudgetCategory::TradeExport);
    let summary = format!(
        "{population} residents, {unemployment:.1}% unemployment, {l2} L2 and {l3} L3 homes, \
        {unserved} buildings without utilities, {}/{} W power, {:.0}/{:.0} water, {:.0}/{:.0} sewage, \
        {bus_trips} completed bus trips, {} buses, traffic on {traffic_roads} roads, \
        treasury {treasury}, last full day {balance}, freight {freight:?} \
        (imports {}, exports {})",
        power.produced_power.watts(), power.consumed_power.watts(), water.water_production,
        water.water_demand, water.sewage_capacity, water.sewage_demand, transit.runs.len(),
        Money::new_inner(imports as i64), Money::new_inner(exports as i64),
    );
    if population < 300
        || unemployed * 100 >= population * 35
        || l2 == 0
        || l3 == 0
        || unserved != 0
        || power.produced_power <= power.consumed_power
        || water.water_production <= water.water_demand
        || water.sewage_capacity <= water.sewage_demand
        || !both_loops_running
        || bus_trips == 0
        || traffic_roads == 0
        || treasury <= Money::ZERO
        || daily_balance <= 0
        || imports >= 0
        || exports <= 0
    {
        return Err(format!(
            "Showcase City did not meet its playtest gates: {summary}"
        ));
    }
    println!("Showcase City: {summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use simulation::map::ZoneGrid;

    #[test]
    fn demo_cities_starter_has_zoning_without_advancing_growth() {
        let starter = build_starter().unwrap();
        let zones = starter.sim.read::<ZoneGrid>();
        assert_eq!(
            zones
                .cells
                .values()
                .map(|cell| cell.kind)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([Residential, Commercial, Industrial]),
        );
        assert!(zones.cells.values().all(|cell| cell.density == Low));
        assert!(starter.sim.world().humans.is_empty());
        assert_eq!(starter.sim.world().trains.len(), 1);
        assert!(starter.sim.get_tick() < TICKS_PER_HOUR);
    }
}

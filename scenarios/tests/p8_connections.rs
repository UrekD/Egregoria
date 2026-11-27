//! Wave 0 evidence tests. See scenarios/fixtures/p8/README.md for invocations.

#[path = "p8_connections/p8_slots.rs"]
mod p8_slots;

use geom::{vec2, Vec2, Vec3};
use scenarios::builders::{grid_city, place_company};
use scenarios::ScenarioCtx;
use serde::Serialize;
use simulation::economy::cargo::CargoBook;
use simulation::map::{BuildingID, LaneKind};
use simulation::map_dynamic::transit::{JourneyStats, RailSchedule, RailTransit};
use simulation::map_dynamic::{Destination, LineID, Router, RoutingStep, StopID, Transit};
use simulation::souls::human::{spawn_human, HumanDecisionKind};
use simulation::transportation::Location;
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{Simulation, SimulationOptions};

const ORIGIN: Vec2 = vec2(600.0, 600.0);
const SPACING: f32 = 120.0;
const CUTOFF: u64 = 18_000;

fn apply(sim: &mut Simulation, commands: &[WorldCommand]) {
    sim.apply_commands(commands);
    let outcomes = sim.read::<CommandOutcomes>();
    assert!(outcomes.last.is_empty(), "{:?}", outcomes.last);
    assert_eq!(outcomes.accepted.len(), commands.len());
}

fn line(ctx: &mut ScenarioCtx, edges: &[([u8; 2], [u8; 2])], first_offset: f32) -> LineID {
    let mut stops = Vec::new();
    for (index, &(from, to)) in edges.iter().enumerate() {
        let command = {
            let map = ctx.sim.map();
            let point = |[x, y]: [u8; 2]| ORIGIN + vec2(f32::from(x), f32::from(y)) * SPACING;
            let lane = map
                .lanes()
                .values()
                .filter(|lane| lane.kind == LaneKind::Driving)
                .find(|lane| {
                    map.intersections()[lane.src]
                        .pos
                        .xy()
                        .is_close(point(from), 1.0)
                        && map.intersections()[lane.dst]
                            .pos
                            .xy()
                            .is_close(point(to), 1.0)
                })
                .unwrap();
            let offset = if index == 0 { first_offset } else { 0.0 };
            let (pos, dir) = lane
                .points
                .point_dir_along(lane.points.length() / 2.0 + offset);
            WorldCommand::TransitAddStop { pos, dir: dir.xy() }
        };
        let before = ctx.sim.read::<Transit>().stops.keys().collect::<Vec<_>>();
        apply(&mut ctx.sim, &[command]);
        stops.push(
            ctx.sim
                .read::<Transit>()
                .stops
                .keys()
                .find(|stop| !before.contains(stop))
                .unwrap(),
        );
    }
    let before = ctx.sim.read::<Transit>().lines.keys().collect::<Vec<_>>();
    apply(&mut ctx.sim, &[WorldCommand::TransitCreateLine { stops }]);
    let line = ctx
        .sim
        .read::<Transit>()
        .lines
        .keys()
        .find(|id| !before.contains(id))
        .unwrap();
    apply(
        &mut ctx.sim,
        &[WorldCommand::TransitSetVehicles { line, n: 4 }],
    );
    line
}

fn house(sim: &mut Simulation, near: Vec2) -> BuildingID {
    let lot = sim
        .map()
        .lots()
        .values()
        .min_by(|a, b| {
            a.shape
                .center()
                .distance2(near)
                .total_cmp(&b.shape.center().distance2(near))
        })
        .unwrap()
        .id;
    let before: Vec<_> = sim.map().buildings().keys().collect();
    apply(sim, &[WorldCommand::MapBuildHouse(lot)]);
    sim.map()
        .buildings()
        .keys()
        .find(|id| !before.contains(id))
        .unwrap()
}

#[derive(Serialize)]
struct BusOfferTrace {
    offer_id: &'static str,
    origin: BuildingID,
    origin_pos: Vec3,
    final_door: BuildingID,
    final_door_pos: Vec3,
    active_buses_at_offer: usize,
    offered_tick: u64,
    cutoff_tick: u64,
    waits: Vec<(u64, LineID, StopID)>,
    boardings: Vec<(u64, LineID)>,
    alightings: Vec<u64>,
    arrived_tick: Option<u64>,
}

fn bus_offer(cross_line: bool, with_service: bool, opposite_sidewalks: bool) -> BusOfferTrace {
    let mut ctx = ScenarioCtx::new(SimulationOptions {
        save_replay: false,
        terrain_size: 0,
        ..SimulationOptions::LEGACY
    });
    ctx.sim = Simulation::new_authored_flat(10, 0).unwrap();
    assert_eq!(grid_city(&mut ctx, ORIGIN, 5, 2, SPACING).roads.len(), 13);
    let west = line(
        &mut ctx,
        &[([0, 0], [1, 0]), ([2, 0], [2, 1]), ([1, 1], [0, 1])],
        0.0,
    );
    let east = line(
        &mut ctx,
        &[
            if opposite_sidewalks { ([2, 1], [2, 0]) } else { ([2, 0], [2, 1]) },
            ([3, 0], [4, 0]),
            ([4, 1], [3, 1]),
        ],
        if opposite_sidewalks { 0.0 } else { 10.0 },
    );
    if !with_service {
        apply(
            &mut ctx.sim,
            &[
                WorldCommand::TransitSetVehicles { line: west, n: 0 },
                WorldCommand::TransitSetVehicles { line: east, n: 0 },
            ],
        );
    }
    ctx.run_ticks(800);
    let from = house(&mut ctx.sim, ORIGIN + vec2(60.0, -25.0));
    let to = house(
        &mut ctx.sim,
        ORIGIN
            + if cross_line {
                vec2(420.0, -25.0)
            } else {
                vec2(265.0, 60.0)
            },
    );
    let rider = spawn_human(&mut ctx.sim, from).unwrap();
    let target = Destination::Building(to);
    let human = ctx.sim.world_mut_unchecked().humans.get_mut(rider).unwrap();
    human.pedestrian.walking_speed = 1.25;
    human.router = Router::new(None);
    human.router.go_to(target);
    human.decision.kind = HumanDecisionKind::GoTo(target);
    let offered_tick = ctx.sim.get_tick();
    let mut trace = BusOfferTrace {
        offer_id: if cross_line {
            "west-to-east"
        } else {
            "west-to-interchange"
        },
        origin: from,
        origin_pos: ctx.sim.map().get(from).unwrap().door_pos,
        final_door: to,
        final_door_pos: ctx.sim.map().get(to).unwrap().door_pos,
        active_buses_at_offer: ctx.sim.read::<Transit>().runs.len(),
        offered_tick,
        cutoff_tick: offered_tick + CUTOFF,
        waits: Vec::new(),
        boardings: Vec::new(),
        alightings: Vec::new(),
        arrived_tick: None,
    };
    let mut previous_bus = None;
    let mut previous_wait = None;
    for now in trace.offered_tick + 1..=trace.cutoff_tick {
        ctx.run_ticks(1);
        let human = &ctx.sim.world().humans[rider];
        let current_bus = match human.location {
            Location::Transit(vehicle) => Some(vehicle),
            Location::Vehicle(_) => panic!("the carless offer boarded a private vehicle"),
            _ => None,
        };
        let waiting = match human.router.cur_step() {
            Some(RoutingStep::WaitAtStop { line, stop, .. }) => Some((*line, *stop)),
            _ => None,
        };
        if waiting != previous_wait {
            if let Some((line, stop)) = waiting {
                trace.waits.push((now, line, stop));
            }
            previous_wait = waiting;
        }
        if current_bus != previous_bus {
            if previous_bus.is_some() {
                trace.alightings.push(now);
            }
            if let Some(vehicle) = current_bus {
                let line = ctx.sim.read::<Transit>().runs[&vehicle].line;
                assert!(human.collider.is_none());
                trace.boardings.push((now, line));
            }
        }
        if human.location == Location::Building(to) {
            trace.arrived_tick = Some(now);
            break;
        }
        previous_bus = current_bus;
    }
    println!("bus_offer {}", serde_json::to_string(&trace).unwrap());
    trace
}

#[test]
fn p8_bus_stock_reduction_controls() {
    let walking = bus_offer(true, false, false);
    assert!(walking.arrived_tick.is_some() && walking.boardings.is_empty());
    let single = bus_offer(false, true, false);
    assert_eq!(
        single.boardings.len(),
        1,
        "the useful single line must carry the rider"
    );
    assert_eq!(single.alightings.len(), 1);
    assert!(single.arrived_tick.is_some());
}

#[test]
fn p8_bus_stock_reduction() {
    let trace = bus_offer(true, true, false);
    assert!(
        trace.boardings.len() >= 2,
        "the offer completed fewer than two boardings"
    );
    assert_ne!(trace.boardings[0].1, trace.boardings[1].1);
    assert_eq!(trace.alightings.len(), 2);
    let [first, second] = trace.alightings[..] else {
        unreachable!()
    };
    assert!(trace.boardings[0].0 < first && first < trace.boardings[1].0);
    assert!(trace.boardings[1].0 < second && second < trace.arrived_tick.unwrap());
}

#[test]
fn p8_bus_pinned_opposite_sidewalks_respect_crossing_detour() {
    let trace = bus_offer(true, true, true);
    assert!(trace.arrived_tick.is_some());
    assert_eq!(trace.boardings.len(), 1);
}

#[test]
fn p8_stock_warehouse_policy_control() {
    use prototypes::{try_prototype, WarehousePolicy};
    use simulation::economy::cargo::{DepotRestock, Endpoint, FreightTrace};
    use simulation::world_command::Rejection;
    let mut ctx = ScenarioCtx::new(SimulationOptions {
        save_replay: false,
        terrain_size: 0,
        ..SimulationOptions::LEGACY
    });
    ctx.sim = Simulation::new_authored_flat(10, 0).unwrap();
    grid_city(&mut ctx, ORIGIN, 2, 2, SPACING);
    let building = place_company(&mut ctx, "small-depot", ORIGIN + vec2(140.0, -30.0))
        .expect("the stock warehouse must fit");
    let (company, data) = ctx
        .sim
        .world()
        .companies
        .iter()
        .find(|(_, company)| company.comp.building == building)
        .unwrap();
    let spec = try_prototype(data.comp.proto)
        .unwrap()
        .storage
        .as_ref()
        .unwrap();
    let configured = |sim: &Simulation| sim.read::<CargoBook>().warehouse_policy(company, spec);
    assert_eq!(configured(&ctx.sim), spec.default_policy);
    assert_ne!(spec.default_policy, WarehousePolicy::ReserveLocally);
    apply(
        &mut ctx.sim,
        &[WorldCommand::SetWarehousePolicy {
            building,
            policy: WarehousePolicy::ReserveLocally,
        }],
    );
    assert_eq!(configured(&ctx.sim), WarehousePolicy::ReserveLocally);
    let depot = Endpoint {
        soul: simulation::SoulID::GoodsCompany(company),
        building,
    };
    let limits = DepotRestock { low: 1, high: 2 };
    let command = |expected_revision, limits| WorldCommand::SetWarehouseRestock {
        depot,
        expected_revision,
        limits,
    };
    assert_eq!(
        command(0, DepotRestock { low: 2, high: 1 })
            .preview(&ctx.sim)
            .unwrap_err(),
        Rejection::InvalidFreightPolicy
    );
    apply(&mut ctx.sim, &[command(0, limits)]);
    let setting = ctx.sim.read::<FreightTrace>().depot_restock(company, spec);
    assert_eq!(setting.revision, 1);
    assert_eq!(setting.limits, limits);
    assert_eq!(
        command(0, limits).preview(&ctx.sim).unwrap_err(),
        Rejection::StaleFreightPolicy
    );
}

#[test]
fn p8_rail_passenger_station_can_be_built_through_commands() {
    use geom::OBB;
    use prototypes::BuildingGen;
    use simulation::map::BuildingKind;

    let mut ctx = ScenarioCtx::new(SimulationOptions::LEGACY);
    ctx.sim = Simulation::new_authored_flat(10, 0).unwrap();
    grid_city(&mut ctx, ORIGIN, 2, 2, SPACING);
    let road = ctx
        .sim
        .map()
        .nearest_lane(
            (ORIGIN + vec2(60.0, 0.0)).z0(),
            LaneKind::Walking,
            Some(30.0),
        )
        .map(|lane| ctx.sim.map().lanes()[lane].parent)
        .unwrap();
    apply(
        &mut ctx.sim,
        &[WorldCommand::MapBuildSpecialBuilding {
            pos: OBB::new(ORIGIN + vec2(60.0, -16.0), -Vec2::Y, 12.0, 20.0),
            kind: BuildingKind::TrainStation,
            gen: BuildingGen::House,
            zone: None,
            connected_road: Some(road),
        }],
    );
    assert!(ctx
        .sim
        .map()
        .buildings()
        .values()
        .any(|building| building.kind == BuildingKind::TrainStation));
}

fn rail_fixture(schedule: RailSchedule) -> (ScenarioCtx, LineID) {
    passenger_rail_fixture(schedule, false).unwrap()
}

fn offer_carless_trip(sim: &mut Simulation, rider: simulation::HumanID, to: BuildingID) {
    let human = sim.world_mut_unchecked().humans.get_mut(rider).unwrap();
    let destination = Destination::Building(to);
    human.pedestrian.walking_speed = 1.25;
    human.router = Router::new(None);
    human.router.go_to(destination);
    human.decision.kind = HumanDecisionKind::GoTo(destination);
}

fn rail_project(
    sim: &Simulation,
    point: Vec2,
    radius: f32,
    filter: simulation::map::ProjectFilter,
) -> simulation::map::MapProject {
    let map = sim.map();
    let elevation = map.environment().height(point).unwrap();
    map.project(point.z(elevation), radius, filter)
}

fn passenger_rail_fixture(
    schedule: RailSchedule,
    across_road: bool,
) -> Result<(ScenarioCtx, LineID), simulation::world_command::Rejection> {
    use geom::OBB;
    use prototypes::{BuildingGen, RollingStockID};
    use simulation::map::{BuildingKind, LanePatternBuilder, ProjectFilter};
    use simulation::map_dynamic::transit::RailPlatformSpec;
    use simulation::world_command::Rejection;

    let mut ctx = ScenarioCtx::new(SimulationOptions::LEGACY);
    ctx.sim = Simulation::new_authored_flat(10, 0).unwrap();
    let (west, east, south, north) = (550.0, 1_150.0, 600.0, 1_000.0);
    let road_pattern = LanePatternBuilder::new().build();
    let rail_pattern = LanePatternBuilder::new().rail(true).one_way(true).build();
    let mut connect = |from: Vec2, to: Vec2, rail: bool| {
        let endpoint = |pos| rail_project(&ctx.sim, pos, 1.0, ProjectFilter::INTER);
        let (from, to) = (endpoint(from), endpoint(to));
        let command = WorldCommand::MapMakeConnection {
            from,
            to,
            inter: None,
            pat: if rail {
                rail_pattern.clone()
            } else {
                road_pattern.clone()
            },
        };
        apply(&mut ctx.sim, &[command]);
    };
    for y in [south, north] {
        connect(vec2(west + 25.0, y), vec2(east - 25.0, y), false);
    }
    let platform_setback = 50.0;
    let corners = [
        vec2(west, south + platform_setback),
        vec2(east, south + platform_setback),
        vec2(east, north - platform_setback),
        vec2(west, north - platform_setback),
    ];
    for (&from, &to) in corners.iter().zip(corners.iter().cycle().skip(1)) {
        connect(from, to, true);
    }
    // Provide crosswalks before placing stations and issuing offers.
    for (x, y, end_y) in [(800.0, south, 520.0), (900.0, north, 1080.0)] {
        let from = rail_project(&ctx.sim, vec2(x, y), 5.0, ProjectFilter::ROAD);
        let to = rail_project(&ctx.sim, vec2(x, end_y), 1.0, ProjectFilter::INTER);
        apply(&mut ctx.sim, &[WorldCommand::MapMakeConnection {
            from,
            to,
            inter: None,
            pat: road_pattern.clone(),
        }]);
    }

    let mut platforms = Vec::new();
    for (road_y, track_y, side) in [
        (south, south + platform_setback, 1.0),
        (north, north - platform_setback, -1.0),
    ] {
        let side = if across_road { -side } else { side };
        let x = 850.0;
        let road = ctx
            .sim
            .map()
            .nearest_lane(vec2(x, road_y).z0(), LaneKind::Walking, Some(30.0))
            .map(|lane| ctx.sim.map().lanes()[lane].parent)
            .unwrap();
        apply(
            &mut ctx.sim,
            &[WorldCommand::MapBuildSpecialBuilding {
                pos: OBB::new(vec2(x, road_y + side * 16.0), Vec2::Y * side, 12.0, 20.0),
                kind: BuildingKind::TrainStation,
                gen: BuildingGen::House,
                zone: None,
                connected_road: Some(road),
            }],
        );
        let spec = {
            let map = ctx.sim.map();
            let station = map
                .buildings()
                .values()
                .filter(|b| b.kind == BuildingKind::TrainStation)
                .min_by_key(|b| ((b.door_pos.y - road_y).abs() as u32, b.id))
                .unwrap()
                .id;
            let lane = map
                .nearest_lane(vec2(x, track_y).z0(), LaneKind::Rail, Some(30.0))
                .unwrap();
            let track = &map.lanes()[lane];
            RailPlatformSpec {
                station,
                lane,
                length: 50.0,
                offset: track
                    .points
                    .length_at_proj(track.points.project(vec2(x, track_y).z0())),
            }
        };
        assert_eq!(
            WorldCommand::TransitAddRailPlatform {
                platform: RailPlatformSpec {
                    offset: 20.0,
                    ..spec
                }
            }
            .preview(&ctx.sim)
            .unwrap_err(),
            Rejection::InvalidConnection,
        );
        let add = WorldCommand::TransitAddRailPlatform { platform: spec };
        if across_road {
            add.preview(&ctx.sim)?;
        }
        apply(&mut ctx.sim, &[add]);
        platforms.push(
            *ctx.sim
                .read::<RailTransit>()
                .platforms
                .keys()
                .last()
                .unwrap(),
        );
    }

    let consist = vec![RollingStockID::new("passenger-emu-front")];
    apply(
        &mut ctx.sim,
        &[WorldCommand::TransitCreateRailLine {
            calls: platforms,
            consist,
            schedule,
        }],
    );
    let line = *ctx.sim.read::<RailTransit>().lines.keys().next().unwrap();
    Ok((ctx, line))
}

#[test]
fn p8_rail_rejects_unmarked_cross_road_connector() {
    assert!(matches!(
        passenger_rail_fixture(RailSchedule::default(), true),
        Err(simulation::world_command::Rejection::InvalidConnection),
    ));
}

#[test]
fn p8_rail_recurring_zero_lateness_departures() {
    let schedule = RailSchedule {
        max_late_ticks: 0,
        ..RailSchedule::default()
    };
    let (mut ctx, _) = rail_fixture(schedule);
    let mut was_en_route = false;
    let mut departures = Vec::new();
    for _ in 0..CUTOFF {
        ctx.run_ticks(1);
        let en_route = ctx.sim.read::<RailTransit>().runs.values().any(|run| {
            run.next_platform_index() == 1 && run.dwell_until().is_none()
        });
        if en_route && !was_en_route {
            departures.push(simulation::map_dynamic::transit::simulation_elapsed_ticks(&ctx.sim).0);
        }
        if departures.len() == 2 {
            break;
        }
        was_en_route = en_route;
    }
    assert_eq!(departures.len(), 2);
    for departure in departures {
        assert_eq!(departure % schedule.headway_ticks, 0, "departure {departure}");
    }
}

#[test]
fn p8_rail_bus_connections_in_both_directions() {
    use simulation::map::{LanePatternBuilder, ProjectFilter};

    // The feeder buses can bunch at the first stop. Allow another circuit
    // for riders whose access walk misses the opening departure.
    const COHORT_CUTOFF: u64 = 50_000;
    let (mut ctx, _) = rail_fixture(RailSchedule::default());
    let from = rail_project(&ctx.sim, vec2(800.0, 520.0), 1.0, ProjectFilter::INTER);
    let to = rail_project(&ctx.sim, vec2(800.0, 120.0), 1.0, ProjectFilter::INTER);
    apply(&mut ctx.sim, &[WorldCommand::MapMakeConnection {
        from,
        to,
        inter: None,
        pat: LanePatternBuilder::new().build(),
    }]);
    let from = rail_project(&ctx.sim, vec2(100.0, 230.0), 1.0, ProjectFilter::INTER);
    let to = rail_project(&ctx.sim, vec2(800.0, 230.0), 5.0, ProjectFilter::ROAD);
    apply(&mut ctx.sim, &[WorldCommand::MapMakeConnection {
        from,
        to,
        inter: None,
        pat: LanePatternBuilder::new().build(),
    }]);
    let mut stops = Vec::new();
    for (point, direction) in [
        (vec2(250.0, 230.0), Vec2::X),
        (vec2(865.0, 600.0), -Vec2::X),
        (vec2(250.0, 230.0), -Vec2::X),
    ] {
        let add = {
            let map = ctx.sim.map();
            let point = point.z(map.environment().height(point).unwrap());
            let lane = map.lanes().values()
                .filter(|lane| lane.kind == LaneKind::Driving)
                .filter(|lane| lane.points.project_segment_dir(point).2.xy().dot(direction) > 0.9)
                .min_by(|a, b| a.points.project_dist(point).total_cmp(&b.points.project_dist(point)))
                .unwrap();
            WorldCommand::TransitAddStop {
                pos: lane.points.project(point),
                dir: direction,
            }
        };
        apply(&mut ctx.sim, &[add]);
        stops.push(ctx.sim.read::<Transit>().stops.keys().last().unwrap());
    }
    apply(&mut ctx.sim, &[WorldCommand::TransitCreateLine { stops }]);
    let bus = ctx.sim.read::<Transit>().lines.keys().last().unwrap();
    apply(&mut ctx.sim, &[WorldCommand::TransitSetVehicles { line: bus, n: 4 }]);

    let destination = house(&mut ctx.sim, vec2(930.0, 980.0));
    let mut cohort = Vec::new();
    for _ in 0..20 {
        let home = house(&mut ctx.sim, vec2(250.0, 230.0));
        cohort.push((spawn_human(&mut ctx.sim, home).unwrap(), home));
    }
    ctx.run_ticks(950);
    for outward in [true, false] {
        let mut trips = Vec::new();
        for &(id, home) in &cohort {
            let building = if outward { destination } else { home };
            offer_carless_trip(&mut ctx.sim, id, building);
            trips.push((id, building, Vec::new(), 0, false));
        }
        for _ in 0..COHORT_CUTOFF {
            ctx.run_ticks(1);
            for (id, dest, modes, previous, arrived) in trips.iter_mut().filter(|trip| !trip.4) {
                let h = ctx.sim.get(*id).unwrap();
                let mode = match h.location {
                    Location::Transit(_) => 1,
                    Location::RailTransit(_) => 2,
                    _ => 0,
                };
                if mode != 0 && *previous != mode {
                    modes.push(mode);
                }
                *previous = mode;
                *arrived = h.location == Location::Building(*dest);
            }
            if trips.iter().all(|trip| trip.4) {
                break;
            }
        }
        let expected = if outward { [1, 2] } else { [2, 1] };
        let arrived = trips.iter().filter(|trip| trip.4).count();
        let transferred = trips.iter().filter(|trip| trip.4 && trip.2.as_slice() == expected).count();
        println!("rail cohort outward={outward} offered={} arrived={arrived} transferred={transferred}", trips.len());
        assert!(arrived >= 16 && transferred >= 5, "{trips:?}");
    }
}

#[test]
fn p8_rail_out_and_back_with_deterministic_resume() {
    use prototypes::Money;
    use simulation::economy::{Budget, BudgetCategory};

    let (mut ctx, line) = rail_fixture(RailSchedule::default());
    apply(
        &mut ctx.sim,
        &[WorldCommand::TransitSetFare {
            line,
            fare: Money::new_bucks(2),
        }],
    );
    use simulation::map::road_edit::{preview_road_upgrade, road_edit_stamp, RoadUpgrade};
    use simulation::map::{LanePatternBuilder, MapProject, ProjectKind};
    use simulation::world_command::PrepareBudget;

    let (narrow, expected, change) = {
        let stop = ctx.sim.read::<Transit>().lines[line].stops[1];
        let platform = ctx.sim.read::<RailTransit>().platforms[&stop];
        let station = ctx.sim.map().get(platform.station).unwrap().door_pos;
        let berth = ctx.sim.read::<Transit>().stops[stop].berth;
        let middle = (station + berth) * 0.5;
        let endpoint = |dx, dy| MapProject {
            pos: middle + Vec3::X * dx + Vec3::Y * dy,
            kind: ProjectKind::Ground,
        };
        apply(
            &mut ctx.sim,
            &[WorldCommand::MapMakeConnection {
                from: endpoint(12.0, -12.0),
                to: endpoint(12.0, 12.0),
                inter: None,
                pat: LanePatternBuilder::new()
                    .one_way(true)
                    .parking(false)
                    .sidewalks(false)
                    .build(),
            }],
        );
        let narrow = ctx.sim.map().roads().keys().last().unwrap();
        let change = RoadUpgrade::Replace(LanePatternBuilder::new().n_lanes(3).build());
        let expected =
            road_edit_stamp(&ctx.sim, narrow, &mut PrepareBudget::for_preview(100_000)).unwrap();
        (narrow, expected, change)
    };
    let quote = preview_road_upgrade(
        &ctx.sim,
        narrow,
        expected,
        &change,
        &mut PrepareBudget::for_preview(100_000),
    )
    .unwrap()
    .quote;
    let widen = WorldCommand::MapUpgradeRoad {
        road: narrow,
        expected,
        change,
        quote,
    };

    let from = house(&mut ctx.sim, vec2(750.0, 625.0));
    let to = house(&mut ctx.sim, vec2(930.0, 980.0));
    let human = spawn_human(&mut ctx.sim, from).unwrap();
    for _ in 0..2_000 {
        ctx.run_ticks(1);
        if !ctx.sim.read::<RailTransit>().runs.is_empty() {
            break;
        }
    }
    assert_eq!(ctx.sim.read::<RailTransit>().runs.len(), 1);
    offer_carless_trip(&mut ctx.sim, human, to);
    for _ in 0..CUTOFF {
        ctx.run_ticks(1);
        if matches!(
            ctx.sim.get(human).unwrap().location,
            Location::RailTransit(_)
        ) {
            break;
        }
    }
    let rider = ctx.sim.get(human).unwrap();
    assert!(
        matches!(rider.location, Location::RailTransit(_)),
        "{:?} {:?}",
        rider.location,
        rider.router.cur_step()
    );
    assert!(rider.collider.is_none());
    for _ in 0..1_500 {
        if ctx.sim.read::<RailTransit>().runs.values().any(|run| {
            run.next_platform_index() == 1 && run.dwell_until().is_none()
        }) {
            break;
        }
        ctx.run_ticks(1);
    }
    assert!(ctx.sim.read::<RailTransit>().runs.values().any(|run| {
        run.next_platform_index() == 1 && run.dwell_until().is_none()
    }));
    let destination_stop = ctx.sim.read::<Transit>().lines[line].stops[1];
    let platform = ctx.sim.read::<RailTransit>().platforms[&destination_stop];
    let road = ctx.sim.map().get(platform.lane).unwrap().parent;
    let before = ctx.sim.hashes();
    assert_eq!(
        widen.preview(&ctx.sim).unwrap_err(),
        simulation::world_command::Rejection::OccupiedTrack,
    );
    ctx.sim.apply_commands([&widen]);
    assert_eq!(
        ctx.sim.read::<CommandOutcomes>().last.as_slice(),
        &[(
            "MapUpgradeRoad",
            simulation::world_command::Rejection::OccupiedTrack
        ),]
    );
    assert_eq!(before, ctx.sim.hashes());
    assert_eq!(
        WorldCommand::MapRemoveRoad(road).preview(&ctx.sim).unwrap_err(),
        simulation::world_command::Rejection::OccupiedTrack,
    );
    assert_eq!(before, ctx.sim.hashes());
    ctx.sim.apply_commands([&WorldCommand::MapRemoveRoad(road)]);
    assert_eq!(ctx.sim.read::<CommandOutcomes>().last.as_slice(), &[
        ("MapRemoveRoad", simulation::world_command::Rejection::OccupiedTrack),
    ]);
    assert_eq!(before, ctx.sim.hashes());
    let crossing = {
        use simulation::map::{LanePatternBuilder, MapProject, ProjectKind};

        let map = ctx.sim.map();
        let station = map.get(platform.station).unwrap();
        let berth = ctx.sim.read::<Transit>().stops[destination_stop].berth;
        let middle = (station.door_pos + berth) * 0.5;
        let endpoint = |dx| MapProject {
            pos: middle + Vec3::X * dx,
            kind: ProjectKind::Ground,
        };
        WorldCommand::MapMakeConnection {
            from: endpoint(-60.0),
            to: endpoint(60.0),
            inter: None,
            pat: LanePatternBuilder::new().build(),
        }
    };
    assert_eq!(
        crossing.preview(&ctx.sim).unwrap_err(),
        simulation::world_command::Rejection::OccupiedTrack,
    );
    ctx.sim.apply_commands([&crossing]);
    assert_eq!(before, ctx.sim.hashes());
    let WorldCommand::MapMakeConnection {
        from: road_start,
        to: road_end,
        pat,
        ..
    } = crossing
    else {
        unreachable!()
    };
    let far_away = |mut endpoint: simulation::map::MapProject| {
        endpoint.pos.x += 400.0;
        endpoint
    };
    apply(&mut ctx.sim, &[WorldCommand::MapMakeConnection {
        from: far_away(road_start),
        to: far_away(road_end),
        inter: None,
        pat,
    }]);
    let bytes = ctx
        .sim
        .encode_slot(simulation::SnapshotRevision::new(1))
        .unwrap()
        .into_snapshot_bytes();
    let mut resumed = Simulation::load_compressed_snapshot(&bytes, 32 * 1024 * 1024).unwrap();
    assert_eq!(ctx.sim.hashes(), resumed.hashes());
    let mut resume_schedule = Simulation::schedule();
    for _ in 0..CUTOFF {
        ctx.run_ticks(1);
        resumed.tick(&mut resume_schedule, &[]);
        if ctx.sim.get(human).unwrap().location == Location::Building(to) {
            break;
        }
    }
    assert_eq!(ctx.sim.get(human).unwrap().location, Location::Building(to));
    assert_eq!(ctx.sim.hashes(), resumed.hashes());
    // The return is a new door-to-door offer with a fresh trip state.
    offer_carless_trip(&mut ctx.sim, human, from);
    for _ in 0..CUTOFF {
        ctx.run_ticks(1);
        if ctx.sim.get(human).unwrap().location == Location::Building(from) {
            break;
        }
    }
    assert_eq!(
        ctx.sim.get(human).unwrap().location,
        Location::Building(from)
    );
    let stats = ctx.sim.read::<JourneyStats>().line(line);
    assert_eq!(
        (stats.boardings, stats.alightings, stats.transfers),
        (2, 2, 0)
    );
    assert!(ctx.sim.read::<Budget>().total(BudgetCategory::TransitFares) > 0);
}

#[test]
fn p8_rail_removal_drains_a_rider_at_the_next_platform() {
    let (mut ctx, line) = rail_fixture(RailSchedule::default());
    let from = house(&mut ctx.sim, vec2(750.0, 625.0));
    let to = house(&mut ctx.sim, vec2(930.0, 980.0));
    let rider = spawn_human(&mut ctx.sim, from).unwrap();
    ctx.run_ticks(950);
    offer_carless_trip(&mut ctx.sim, rider, to);
    for _ in 0..CUTOFF {
        ctx.run_ticks(1);
        if let Location::RailTransit(train) = ctx.sim.get(rider).unwrap().location {
            let rail = ctx.sim.read::<RailTransit>();
            let run = &rail.runs[&train];
            if run.next_platform_index() == 1 && run.dwell_until().is_none() {
                break;
            }
        }
    }
    assert!(matches!(
        ctx.sim.get(rider).unwrap().location,
        Location::RailTransit(_)
    ));
    apply(&mut ctx.sim, &[WorldCommand::TransitRemoveLine { line }]);
    assert!(ctx.sim.read::<RailTransit>().lines[&line].is_retiring());

    for _ in 0..CUTOFF {
        ctx.run_ticks(1);
        if ctx.sim.get(rider).unwrap().location == Location::Building(to) {
            break;
        }
    }
    assert_eq!(ctx.sim.get(rider).unwrap().location, Location::Building(to));
    assert!(!ctx.sim.read::<RailTransit>().lines.contains_key(&line));
    assert!(ctx
        .sim
        .read::<RailTransit>()
        .runs
        .values()
        .all(|run| run.line != line));
}

fn shared_freight_rail_arrival(with_exports: bool) -> u64 {
    use geom::OBB;
    use prototypes::{BuildingGen, FreightStationPrototypeID, RollingStockID};
    use simulation::map::{BuildingKind, LanePatternBuilder, Map, ProjectFilter};
    use simulation::map_dynamic::BuildingInfos;
    use simulation::souls::freight_station::{freight_station_soul, FreightTrainState};

    let (mut ctx, _) = rail_fixture(RailSchedule::default());
    // Park the freight consist on a passing track when it finishes its call.
    let rail_pattern = LanePatternBuilder::new().rail(true).one_way(true).build();
    let junction = vec2(1230.0, 800.0);
    for (start, start_filter, end, end_filter) in [
        (
            vec2(1150.0, 700.0),
            ProjectFilter::ROAD,
            junction,
            ProjectFilter::INTER,
        ),
        (
            junction,
            ProjectFilter::INTER,
            vec2(1150.0, 900.0),
            ProjectFilter::ROAD,
        ),
    ] {
        let from = rail_project(&ctx.sim, start, 5.0, start_filter);
        let to = rail_project(&ctx.sim, end, 5.0, end_filter);
        apply(
            &mut ctx.sim,
            &[WorldCommand::MapMakeConnection {
                from,
                to,
                inter: None,
                pat: rail_pattern.clone(),
            }],
        );
    }
    let (gate_lane, gate_offset, gate_pos, gate_dir) = {
        let map = ctx.sim.map();
        let lane = map
            .nearest_lane(vec2(1190.0, 750.0).z0(), LaneKind::Rail, Some(25.0))
            .unwrap();
        let points = &map.get(lane).unwrap().points;
        let offset = points.length() * 0.6;
        let (pos, dir) = points.point_dir_along(offset);
        (lane, offset, pos.xy(), dir.xy().normalize())
    };
    let terminal_pos = vec2(510.0, 875.0);
    let prototype = FreightStationPrototypeID::new("freight-station");
    let [_, site] = [
        (gate_pos, gate_dir, BuildingKind::ExternalTrading, 60.0),
        (
            terminal_pos,
            -Vec2::Y,
            BuildingKind::RailFreightStation(prototype),
            20.0,
        ),
    ]
    .map(|(point, dir, kind, width)| {
        let building = ctx
            .sim
            .write::<Map>()
            .build_special_building(
                &OBB::new(point, dir, width, 16.0),
                kind,
                BuildingGen::NoWalkway { door_pos: point },
                None,
                None,
            )
            .unwrap();
        ctx.sim.write::<BuildingInfos>().insert(building);
        building
    });
    let station = freight_station_soul(&mut ctx.sim, site, prototype).unwrap();
    apply(
        &mut ctx.sim,
        &[WorldCommand::SpawnTrain {
            wagons: ["locomotive", "freight-wagon"]
                .map(RollingStockID::new)
                .to_vec(),
            lane: gate_lane,
            dist: gate_offset,
        }],
    );
    let from = house(&mut ctx.sim, vec2(750.0, 625.0));
    let to = house(&mut ctx.sim, vec2(930.0, 980.0));
    let rider = spawn_human(&mut ctx.sim, from).unwrap();
    ctx.run_ticks(950);
    offer_carless_trip(&mut ctx.sim, rider, to);
    let mut shared_route = false;
    let mut arrival = None;
    for _ in 0..CUTOFF {
        ctx.run_ticks(1);
        let tick = simulation::map_dynamic::transit::simulation_elapsed_ticks(&ctx.sim).0;
        if with_exports && tick == 5_600 {
            // Stage the accepted backlog at the terminal, as in D1's freight fixtures.
            ctx.sim.world_mut_unchecked().freight_stations[station]
                .f
                .waiting_cargo = 20;
        }
        let terminal = &ctx.sim.world().freight_stations[station].f;
        let human = ctx.sim.get(rider).unwrap();
        if human.location == Location::Building(to) {
            arrival.get_or_insert(tick);
        }
        if terminal.waiting_cargo == 0 {
            if let (Location::RailTransit(passenger), Some(&(freight, FreightTrainState::Moving))) =
                (&human.location, terminal.trains.first())
            {
                let passenger = ctx.sim.get(*passenger).unwrap().it.get_route();
                let freight = ctx.sim.get(freight).unwrap().it.get_route();
                shared_route |= passenger.zip(freight).is_some_and(|(passenger, freight)| {
                    std::iter::once(&passenger.cur)
                        .chain(&passenger.reversed_route)
                        .any(|path| freight.cur == *path || freight.reversed_route.contains(path))
                });
            }
        }
        if let Some(arrival) = arrival {
            if !with_exports
                || (shared_route && terminal.trains.is_empty() && terminal.waiting_cargo == 0)
            {
                return arrival;
            }
        }
    }
    panic!("passenger arrival {arrival:?}, shared loaded rail route {shared_route}");
}

#[test]
fn p8_passenger_rides_with_loaded_freight_on_the_same_corridor() {
    let control = shared_freight_rail_arrival(false);
    let with_freight = shared_freight_rail_arrival(true);
    println!("shared corridor passenger arrival ticks: control={control} freight={with_freight}");
    assert!(
        with_freight.saturating_sub(control) <= scenarios::traffic_repair::OD_DELAY_LIMIT_TICKS
    );
}

#[test]
fn p8_rail_track_repair_preserves_the_exit() {
    use simulation::map::{LanePatternBuilder, MapProject, ProjectFilter, ProjectKind};

    let (mut ctx, line) = rail_fixture(RailSchedule::default());
    let pattern = LanePatternBuilder::new().rail(true).one_way(true).build();
    for (y, spur_y) in [(860.0, 820.0), (745.0, 705.0)] {
        let from = rail_project(&ctx.sim, vec2(550.0, y), 5.0, ProjectFilter::ROAD);
        let to = rail_project(&ctx.sim, vec2(490.0, spur_y), 1.0, ProjectFilter::INTER);
        apply(
            &mut ctx.sim,
            &[WorldCommand::MapMakeConnection {
                from,
                to,
                inter: None,
                pat: pattern.clone(),
            }],
        );
    }
    let (return_track, restore) = {
        let map = ctx.sim.map();
        let lane = map
            .nearest_lane(vec2(550.0, 800.0).z0(), LaneKind::Rail, Some(5.0))
            .unwrap();
        let road_id = map.get(lane).unwrap().parent;
        let road = map.get(road_id).unwrap();
        let project = |id: simulation::map::IntersectionID| MapProject {
            pos: map.get(id).unwrap().pos,
            kind: ProjectKind::Intersection(id),
        };
        (
            road_id,
            WorldCommand::MapMakeConnection {
                from: project(road.src),
                to: project(road.dst),
                inter: None,
                pat: pattern,
            },
        )
    };
    let from = house(&mut ctx.sim, vec2(750.0, 625.0));
    let to = house(&mut ctx.sim, vec2(930.0, 980.0));
    let rider = spawn_human(&mut ctx.sim, from).unwrap();
    ctx.run_ticks(950);
    offer_carless_trip(&mut ctx.sim, rider, to);
    let mut moving = false;
    for _ in 0..CUTOFF {
        ctx.run_ticks(1);
        if let Location::RailTransit(id) = ctx.sim.get(rider).unwrap().location {
            let rail = ctx.sim.read::<RailTransit>();
            let run = &rail.runs[&id];
            if run.next_platform_index() == 1 && run.dwell_until().is_none() {
                moving = true;
                break;
            }
        }
    }
    assert!(moving, "passenger never left the origin platform");
    apply(&mut ctx.sim, &[WorldCommand::MapRemoveRoad(return_track)]);
    assert!(
        !ctx.sim.read::<Transit>().lines[line].suspended,
        "the command should queue refresh work"
    );
    ctx.run_ticks(1);
    assert!(ctx.sim.read::<Transit>().lines[line].suspended);

    for _ in 0..CUTOFF {
        ctx.run_ticks(1);
        if ctx.sim.get(rider).unwrap().location == Location::Building(to) {
            break;
        }
    }
    assert_eq!(ctx.sim.get(rider).unwrap().location, Location::Building(to));

    apply(&mut ctx.sim, &[restore]);
    ctx.run_ticks(1);
    assert!(!ctx.sim.read::<Transit>().lines[line].suspended);
    offer_carless_trip(&mut ctx.sim, rider, from);
    for _ in 0..CUTOFF {
        ctx.run_ticks(1);
        if ctx.sim.get(rider).unwrap().location == Location::Building(from) {
            break;
        }
    }
    assert_eq!(
        ctx.sim.get(rider).unwrap().location,
        Location::Building(from)
    );
    assert_eq!(ctx.sim.read::<JourneyStats>().line(line).boardings, 2);
}

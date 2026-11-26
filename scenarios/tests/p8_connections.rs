//! Wave 0 evidence tests. See scenarios/fixtures/p8/README.md for invocations.

#[path = "p8_connections/p8_slots.rs"]
mod p8_slots;

use geom::{vec2, Vec2, Vec3};
use scenarios::builders::{grid_city, place_company};
use scenarios::ScenarioCtx;
use serde::Serialize;
use simulation::economy::cargo::CargoBook;
use simulation::map::{BuildingID, LaneKind};
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

fn line(ctx: &mut ScenarioCtx, edges: &[([u8; 2], [u8; 2])]) -> LineID {
    let mut stops = Vec::new();
    for &(from, to) in edges {
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
            let (pos, dir) = lane.points.point_dir_along(lane.points.length() / 2.0);
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

fn bus_offer(cross_line: bool, with_service: bool) -> BusOfferTrace {
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
    );
    let east = line(
        &mut ctx,
        &[([2, 1], [2, 0]), ([3, 0], [4, 0]), ([4, 1], [3, 1])],
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
    let walking = bus_offer(true, false);
    assert!(walking.arrived_tick.is_some() && walking.boardings.is_empty());
    let single = bus_offer(false, true);
    assert_eq!(
        single.boardings.len(),
        1,
        "the useful single line must carry the rider"
    );
    assert_eq!(single.alightings.len(), 1);
    assert!(single.arrived_tick.is_some());
}

#[test]
#[ignore = "P8 T: the fixed cross-line offer must complete two physical rides"]
fn p8_bus_stock_reduction() {
    let trace = bus_offer(true, true);
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
fn p8_stock_warehouse_policy_control() {
    use prototypes::{try_prototype, WarehousePolicy};
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
}
